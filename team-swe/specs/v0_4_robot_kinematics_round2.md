# Spec · v0.4 候选 2 round 2 — `robot-kinematics-xlerobot` impl + `IkBackend` + S1 wiring + e2e

> v0.4 候选 2 拆**两轮 reviewer round**（v0.4_entry §3.3 拍板）。本
> Spec **只**覆盖**第二轮 round (round 2)**：`robot-kinematics-xlerobot`
> impl + `IkBackend` 第三个真 action backend + S1 coordinator wiring +
> `IkPayload` fbs body 注册链 + `ActionKindProto::ActionKindIk` 三层
> enum append + `ik_backend_smoke` cross-process e2e + summary。
>
> **不**续写 round 1 spec（`team-swe/specs/v0_4_robot_kinematics.md`）；
> round 1 trait crate first release vs round 2 impl + wiring 两轮
> reviewer round 边界清晰，独立文件。
>
> 写于 2026-05-20，commit `a42ef73`（候选 2 round 2 CodeMap pre-research
> land）之后。CodeMap 见
> [`team-swe/codemap/v0_4_robot_kinematics_round2.md`](../codemap/v0_4_robot_kinematics_round2.md)。

---

## What

为 v0.4 候选 2 round 2 落地 IK 路径**真实接入**所需最小可用闭环：

1. **新 workspace member** `crates/robot-kinematics-xlerobot/`：第一个
   per-机器人 IK impl crate；移植 dimos `arm_ik.py` 5-DOF 闭式解到
   Rust（dimos `arm_ik.py` 自身又是从 XLeRobot 上游 `SO101Robot.py`
   `SO101Kinematics.inverse_kinematics` 移植 + 中文 doc 完整化 ——
   `arm_ik.py:95 / :103` 显式标注）；implement round 1 `ArmIkSolver`
   trait（**不**改 trait）；按 feature flag 挂到 `robot-s1`
2. **`IkBackend` 第三个真 action backend**（沿 `HoldPoseBackend` /
   `ScriptedBackend` 同款 `ActionBackend` trait + `inventory::submit!`
   static 注册模板）
3. **S1 coordinator wiring**：`Coordinator` 字段持 IK solver（reviewer
   入口锚定默认 = `Option<Box<dyn ArmIkSolver>>`，备选 `Arc<dyn>` 待
   Q-IK3 拍板）+ `ActionContext` 借用 `Option<&'a dyn ArmIkSolver>`；
   `Coordinator::new()` 启动期注入 + startup fail-fast 策略待 Q-IK10
   拍板
4. **`IkPayload` fbs body + 注册链 6 步**（`SCHEMA_IK_PAYLOAD =
   0x0000_040C` 下一空位已验证；字段集 minimal vs 扩展集待 Q-IK4 拍板）
5. **三层 `ActionKindProto::ActionKindIk` (proto `ACTION_KIND_IK = 4`)
   / `ActionKindFb::Ik = 4` / `ActionKind::Ik` enum append + 下游
   dispatch 映射 atomic land**（沿候选 1 reviewer Plan round 1 阻断
   1 锁定模式：enum append + 映射改动单 commit atomic，避免编译 fail）
6. **`IkError` → `ActionStartError` / `ActionFailure` → `ActionRpcStatus`
   错误映射**（具体映射边界待 Q-IK8 拍板；硬约束 = **0** 扩 frozen
   `ActionFailureKindFb` 5 variant）
7. **`ik_backend_smoke` cross-process e2e binary**（新 feature gate
   `ik-backend-smoke`，沿 `scripted-backend-smoke` / `pubsub-cleanup-
   smoke` / `clear-estop-e2e` 同款模板；phase 集合待 Q-IK13 拍板）

具体 trait 调用形态 / 字段集 / 错误映射细节见 §6 AC + §7 Reviewer
Q-IK1..Q-IK13 + Q-IK16..Q-IK17 待拍板表。

---

## 影响范围

### 必改文件（round 2 land 必须）

| 模块 | 改动 |
|---|---|
| `Cargo.toml`（workspace 根）| `members` 数组按字母序 append `"crates/robot-kinematics-xlerobot"`（位置：按字母序在 `crates/robot-kinematics` 之后、`crates/robot-logging` 之前）+ "未来 crate" 注释段 `robot-kinematics-xlerobot` 行同 commit 清理（沿 round 1 (1/1) 同款）|
| `Cargo.lock` | 新增 `[[package]] name = "robot-kinematics-xlerobot"` stanza + 依赖行（cargo 自动）|
| `crates/robot-kinematics-xlerobot/Cargo.toml`（新）| `[package]` workspace 继承 + `[dependencies] robot-kinematics = { workspace = true }`（impl crate 唯一内部 dep）+ `[lints] workspace = true`；其它 dep（如 `nalgebra` 可选 feature 等）待 Q-IK2 拍板 |
| `crates/robot-kinematics-xlerobot/src/lib.rs`（新）| `XLeRobotArmIkSolver` struct + `impl ArmIkSolver for XLeRobotArmIkSolver` + dimos `arm_ik.py` 5-DOF 闭式解 Rust 移植（`_planar_ik` + `wrist_roll` 平凡映射）+ lib unit（**含** FK round-trip / 工作空间边界 / clamp 行为锚句 / 关节限位 / yaw 处理路径；具体测数待 Plan 拍板）|
| `crates/robot-msg/schemas/ik_payload.fbs`（新）| `IkPayload` fbs body；字段集 minimal vs 扩展集待 Q-IK4 拍板；`namespace robot.msg` 沿现有 schema 同款 |
| `crates/robot-msg/build.rs` | `current_schemas: [&str; 13]` → `[&str; 14]` + append `"ik_payload.fbs"`（沿候选 1 同款数组长度漂移检查）|
| `crates/robot-msg/src/schema_id.rs` | append `pub const SCHEMA_IK_PAYLOAD: u32 = 0x0000_040C;` + `SCHEMA_REGISTRY` 升序末尾 entry（当前 21 → 22 条）|
| `crates/robot-msg/src/markers.rs` | 加 `pub struct IkPayload;` + `impl BodyMarker for IkPayload`（`SCHEMA_ID = schema_id::SCHEMA_IK_PAYLOAD` + `SCHEMA_NAME = "robot.msg.IkPayload"` + `BLOB_EMBEDDED = false` + `type Root<'a> = msg::IkPayload<'a>`；沿 `ScriptedPayload` / `JointState` 同款风格）|
| `crates/robot-msg/src/size_tier.rs` | `default_tier_of` match arm 加 `SCHEMA_IK_PAYLOAD => SizeTier::<待 Q-IK5 拍板>`（CodeMap 倾向 `Control`，未拍板前**不**预决）+ `tests::default_tier_of` 穷举测试自动覆盖（依赖 `SCHEMA_REGISTRY` 遍历）|
| `crates/robot-rpc/proto/robot_control.proto` | `ActionKindProto` enum append `ACTION_KIND_IK = 4`（prost 生成名 `ActionKindProto::ActionKindIk`，沿 `ActionKindScripted` CamelCase + enum prefix 同款）|
| `crates/robot-msg/schemas/action_request.fbs` | `ActionKindFb` enum append `Ik = 4`（不动现有 variant 编号）|
| `crates/robot-s1/src/action.rs` | `ActionKind` enum append `Ik` variant（沿 `DwellMs` / `HoldPose` / `Scripted` 同款）|
| `crates/robot-rpc/src/task_handlers.rs` | `action_kind_proto_to_fb` match arm 加 `ActionKindProto::ActionKindIk => ActionKindFb::Ik`；exhaustive match 不留 `_` 兜底；现有 `EXPECTED_VARIANT_COUNT: usize = 3`（candidate 1 land 锁测）→ 4；可能加 gateway 层 IK payload byte cap（沿候选 1 `MAX_SCRIPTED_PAYLOAD_BYTES` 模板；是否落本 round 2 **如 Q-IK5 size_tier 或 Plan 阶段 payload cap 子项拍板需要**）|
| `crates/robot-s1/src/rr_server/handler.rs` | `ActionKindFb → ActionKind` match arm 加 `ActionKindFb::Ik => ActionKind::Ik`；exhaustive；现有 variant count 锁测同步扩 |
| `crates/robot-s1/src/coordinator.rs` | **条件性改动**（按 Q-IK3 拍板路径落地）：(a)/(b) 路径 → `Coordinator` struct 新字段持 IK solver（`Box<dyn>` 或 `Arc<dyn>`）+ `new()` 签名扩 solver 参数 + tick 期 `ActionContext` 注入；(c) 路径 → 新字段为 `HashMap<ArmId, ...>`（违反 hard boundary，需 reviewer 解锁）；(d) 路径 → **不**改 Coordinator（`IkBackend` 自持 static）。startup fail-fast 路径（待 Q-IK10 拍板）|
| `crates/robot-s1/src/action.rs::ActionContext` | **条件性改动**（按 Q-IK3 拍板路径落地）：(a)/(b) 路径 → 新字段 `pub solver: Option<&'a dyn ArmIkSolver>`；(c) 路径 → 新字段为多 solver 借用 slice / map；(d) 路径 → **0** 改动 |
| `crates/robot-s1/src/action_backend/ik.rs`（新）| `IkBackend` struct + `inventory::submit!` 注册 + `impl ActionBackend for IkBackend`：`name() = "ik"` + `supported_kinds() = &[ActionKind::Ik]` + `supported_schemas() = &[SCHEMA_IK_PAYLOAD]` + `start()` decode `IkPayload` → 调用 solver（具体借用路径按 Q-IK3 拍板：`ctx.solver` (a)/(b)/(c) 或 `IkBackend` 自持 static (d)）→ `solver.solve(target, seed, deadline_ns)` → `IkError` 映射到 `ActionStartError`（待 Q-IK8 拍板）→ 返 `Box<IkInstance>`；single-shot vs trajectory-like 待 Q-IK17 拍板 |
| `crates/robot-s1/src/action_backend/mod.rs` | `pub mod ik;` feature-gated（feature 名待 Q-IK11 拍板）|
| `crates/robot-s1/Cargo.toml` | `[features]` 段加 IK 相关 feature（粒度待 Q-IK11 拍板：`backend-ik` 单一 / `backend-ik` + `kinematics-xlerobot` 双 / `kinematics-<robot>` per-robot 三选项）；如 Q-IK11 选 default-on 则 `default = [..., "backend-ik", ...]` |
| `crates/robot-tests/Cargo.toml` | 新 `[[bin]] name = "ik_backend_smoke"` + `required-features = ["ik-backend-smoke"]` + 新 `ik-backend-smoke` feature 段（沿 `scripted-backend-smoke` 同款）|
| `crates/robot-tests/src/bin/ik_backend_smoke.rs`（新）| cross-process e2e binary；沿 `scripted_backend_smoke.rs` / `pubsub_cleanup_smoke.rs` 同款 CLI args + helper inline 复制模板；phase 集合待 Q-IK13 拍板（CodeMap 倾向 5 phase：selftest / happy_single_target / source_task_id_monotonic / unreachable / invalid_input）|
| `configs/ik_backend_smoke.toml`（新）| XLeRobot IK profile TOML，含 `[hardware] hardware_id = "smoke_arm"` + `[s1.profile.backends] enabled = ["ik", ...]` + IK solver 选型字段（具体 schema 待 Q-IK11 拍板）|

### 可选改动（reviewer Plan round 决定）

| 模块 | 改动 |
|---|---|
| `crates/robot-kinematics-xlerobot/README.md`（新；可选）| 仅 crate 用途 + first release 状态 + 跟 `docs/00_design.md` §4.1 预写文件结构映射 + `XLeRobotArmIkSolver` 一行 API 摘要 + clamp 行为选型锚句；reviewer Plan round 决定是否进 round 2（沿候选 2 round 1 / 候选 1 同款 README 可选）|
| `crates/robot-msg/src/build_helpers.rs` | `build_ik_payload(...)` helper（沿候选 1 拍板 Q5(a) `inline 复制` 倾向；如 reviewer Plan round 改诉求可补）|

### 严禁改动（hard boundary）

下列文件 / 路径 / 语义在 round 2 **零**改动；任一改动 = 触发 reviewer
round 中止 / 走独立 reviewer round 单独拍板：

- **`crates/robot-kinematics/src/lib.rs`** —— round 1 trait crate
  first release **不**改：
  * `ArmIkSolver` trait 方法签名不改（`info()` + `solve(target,
    seed, deadline_ns)` 两方法；如 round 2 实施期发现承不下 dimos
    闭式解必须扩 trait → **走独立 reviewer round amend round 1**，
    **不**在 round 2 内暗改）
  * 5 共享类型字段不改（`Pose` / `Twist` / `Jacobian` / `JointLimit`
    / `KinematicInfo` / `IkSolution`）
  * `IkError` 5 variant 不动（`InvalidInput` / `NoSolution` /
    `OutOfReach` / `SingularityTooClose` / `DeadlineExceeded`）
  * `Cargo.toml [dependencies]` 段不动（仅 `thiserror = {
    workspace = true }`）
- **`BaseKinematics` trait + `BaseKinematicInfo`** —— round 1 Q5(b)
  ACK 推到 round 2 但 round 2 **仍不开**（推到 round 3 / 候选 4 跟
  XLeRobot 三轮全向底盘运动学一起；CodeMap §5 + Q）；本 round 2
  `crates/robot-kinematics/src/lib.rs` **不**加 `BaseKinematics` 类
  型 / trait
- **多臂 dispatch** —— round 1 Q4(X) ACK "多 solver instance" 推未来
  reviewer round；round 2 **单臂先落**；**不**加 `Coordinator::solvers:
  HashMap<ArmId, ...>` / `IkPayload.arm_id` 字段 / per-arm `ArmId`
  类型；如 Q-IK3 拍板 (c) `HashMap` 路径 → reviewer 必须明确 ACK
  解锁本 hard boundary
- **URDF 加载 / frame transform** —— round 2 闭式硬编码（dimos 移植）；
  **不**加 vendor URDF parser / `FrameName` enum / `TransformTree`；
  `IkPayload.frame: string` 字段是否纳入 round 2 待 Q-IK7 拍板，但
  即使纳入也仅作占位字符串（solver 内部假设 shoulder_base frame）；
  实际 frame 变换路径推未来 reviewer round
- **frozen `ActionFailureKindFb`** —— `crates/robot-msg/schemas/
  action_status.fbs:83-91` 现有 5 variant + Unknown 不动（B4 frozen
  schema 边界）；round 2 `IkError` 映射沿用现有 5 variant；**不**加
  第 6 variant `IkSolverError`；如 Q-IK9 拍板必须扩 variant →
  reviewer 必须明确 ACK 解锁本 hard boundary 走独立 reviewer round
  amend `action_status.fbs`
- **`docs/00_design.md` 正文** —— §4.1 / §6.6.4 / §1.5 / §5 / §6
  任一段语义偏离 → 走 `docs/CHANGELOG.md` 偏离登记；round 2 schema
  append + ActionKind append + impl crate 落地都是 §4.1 预写文件结
  构自然外推，**不**走偏离登记
- **§6.7 5 不变量任一条降级 / 升级** —— 5 条全保
- **B3 §3.7 `source_task_id == ctx.command_seq` 单调契约** —— round 2
  `IkBackend` 输出 `JointCommand.source_task_id` **必须** == `ctx.
  command_seq`；**不**用 backend 私有 counter / **不**用 wall clock /
  **不**绕过 `build_hold_command(..., ctx.command_seq, ...)` 或
  `build_joint_command(..., source_task_id, ...)` 既有路径（详 AC-7
  + reviewer 审查重点 #1）
- **B4 frozen wire schema 现有字段编号 / message layout** —— 仅
  append `ACTION_KIND_IK = 4` (proto) + `Ik = 4` (fbs `ActionKindFb`)
  + `SCHEMA_IK_PAYLOAD = 0x0000_040C` + 新 `IkPayload` table；现有
  message / field 编号 / `TaskHandle` / `TaskStatus` / `TaskEvent` /
  `IdempotencyCache` namespace 等全不动
- **候选 1 / 候选 3 已 land 锁定的关键契约** —— `MAX_SCRIPTED_PAYLOAD_
  BYTES` / `CONTROL_TIER_MAX_BYTES` 单源绑定 / `EventsDrainRegistry::
  reap_finished` / shutdown step 4 顺序 / `Box::leak(node)` residual
  全不触
- **`deny.toml`** —— round 2 **不**改 baseline `licenses FAILED`
  follow-up（候选 2 round 1 §P.4 diff #1 follow-up 独立 commit / PR）；
  如 round 2 引入新 license 边界（例如 vendor URDF parser）→ 走 round
  2 Spec amend 拍板 + reviewer round 强 ACK
- **`crates/robot-s1/src/coordinator.rs::Coordinator::new()` 签名**
  ——`solver` 参数加在末尾 / 用 builder 模式 / 直接 setter（具体形
  态待 Plan 拍板）；但不改既有 `config / hardware / layout_id /
  anchor_bias / registry / enabled_backends` 参数顺序 / 类型

---

## 风险等级

**中-高**（vs round 1 "低"）。理由：

- round 2 同时改动 **3 个 crate**（`robot-kinematics-xlerobot` 新 +
  `robot-msg` schema 注册链 + `robot-s1` coordinator/backend/action）
  + **1 个新 binary**（`ik_backend_smoke`）+ **3 层 wire schema**
  （proto + fbs + Rust enum）+ **config TOML**
- 真接入 hot path：tick 期 `IkBackend` 通过 solver（按 Q-IK3 拍板路径
  借用：`ctx.solver` (a)/(b)/(c) 或 `IkBackend::SOLVER` static (d)）调
  `solver.solve(...)` → `build_joint_command(..., ctx.command_seq, ...)`
  → 写
  `JointCommand`；任一环偏离 B3 §3.7 单调契约 / 候选 1 错误映射边界
  / `source_task_id` 注入路径 = 阻断
- `Coordinator::new()` 签名扩展 = breaking change for any non-test
  caller（含 binary entry / smoke harness）；如 binary 入口 / 既有
  smoke 不同步改 → 编译 fail
- IK fail 模型跨 3 层（lib `IkError` → S1 `ActionStartError` /
  `ActionFailure` → gateway `ActionRpcStatus`）；任一层错对 = e2e
  smoke 错码 / 错位
- dimos `arm_ik.py` 5-DOF 闭式解移植 = 跟 round 1 trait `&Pose`
  (xyz + xyzw quat) 接口适配（CodeMap §2.2 gap-1/2/3）；移植期可能
  发现 trait 不够装（→ 走独立 reviewer round amend round 1）
- cross-process e2e binary 新增 = 编译时间 / CI 跑通时间增加
  （`scripted_backend_smoke` 候选 1 实测 33s 编译；`ik_backend_smoke`
  预期同量级）

主要风险点（详 §7 Reviewer Q-IK1..Q-IK13 + Q-IK16..Q-IK17）：

1. **round 1 trait 承不住 xlerobot 闭式解** —— Q-IK6（yaw 处理）+
   Q-IK7（frame 字段）实施期实测撞 trait 边界；硬约束 = 走独立
   reviewer round amend round 1，**不**在 round 2 内暗改
2. **IkPayload 字段集稳定性** —— Q-IK4 拍板字段集后即作 wire schema
   first release；未来扩字段走独立 reviewer round；reviewer 审查重
   点 #3
3. **S1 ownership / fail-fast 形态** —— Q-IK3（4 候选 a/b/c/d）+
   Q-IK10（fail-fast 3 策略 a/b/c）+ Q-IK11（feature flag 粒度）；
   错形态 = 实施期 breaking + 多臂未来扩展成本高
4. **IK fail 映射边界** —— Q-IK8 / Q-IK9；错映射 = e2e smoke 错码 /
   `IkError::OutOfReach` 跟 `Err(InvalidInput)` 混用 / 撞 frozen
   `ActionFailureKindFb` 边界
5. **B3 §3.7 `source_task_id` 单调契约** —— `IkBackend` 输出必须
   `build_*_command(..., ctx.command_seq, ...)` 透传；reviewer 审查
   重点 #1
6. **重型 smoke hard gate 升级** —— round 1 拍 (b) compile gate +
   selftest 是零 runtime 特例；round 2 必须升回 (a) full `--phase
   all` 硬 gate（含本 round 2 新 `ik_backend_smoke --phase all` +
   候选 1 `scripted_backend_smoke --phase all` + 候选 3
   `pubsub_cleanup_smoke --phase all` 三 binary 全过）

---

## 回滚策略

round 2 是**多 crate / 多层 wire schema / hot path 接入**改动；回滚
路径**比 round 1 复杂**但仍走 git revert（**不**回滚到不一致中间态）。

各模块独立 revertable 边界：

- `crates/robot-kinematics-xlerobot/` 整 crate revert（删目录 +
  workspace `members` revert）= 不影响 `robot-kinematics` trait crate
  本身；不影响 `robot-s1` 当前已 build crates（feature gate off 状
  态下）
- schema 注册链 6 步 revert = `current_schemas` 数组 14 → 13 + `SCHEMA
  _IK_PAYLOAD` const 删除 + `SCHEMA_REGISTRY` 升序末尾 entry 删除 +
  `markers.rs` impl 删除 + `size_tier.rs` arm 删除；`schema_lint`
  binary 启动期升序自动校验沿用
- 三层 `ActionKind::Ik` enum append revert = proto / fbs / Rust enum
  + dispatch 映射 5 处 atomic revert（同 (1/n) commit 内做即可，沿
  候选 1 reviewer Plan round 1 阻断 1 锁定模式）
- `Coordinator::new()` 签名扩展 revert = breaking change 反向；如已
  有外部调用方迁移过 → revert 必须同步反向迁移；如未迁移 / 仅 round
  2 内调用 → revert 干净
- `ik_backend_smoke` binary revert = 删 `[[bin]]` + 删 feature + 删
  binary 源；不影响 `scripted_backend_smoke` / `pubsub_cleanup_smoke`
- 任一 lib unit flaky → 标 `#[ignore]` + 留 follow-up issue，不回
  滚整 commit（沿候选 3 / 1 / 2 round 1 同款）

**全局回滚**：如 round 2 land 后发现 wire schema 跟下游不兼容 → 走
独立 follow-up reviewer round 修；本 round 2 commit 链 **不**回滚（避
免破 schema_id 升序约定 / `SCHEMA_REGISTRY` 历史）。

---

## 目标分支 / 交付形式

- 分支：`master`（直接 commit，沿候选 1 / 2 round 1 / 3 模式 + 每段
  reviewer ACK）
- 交付：**commit 节奏 / 拆段顺序留给 Plan 阶段拍板**（reviewer 硬要
  求：Spec 初版 **不**预决 commit 数）；候选拆分参考（**仅**参考；
  reviewer Plan round 拍板）：
  * 路径 A：单 commit (1/1) 全段 atomic land（沿 round 1 同款）—— 适
    用 round 2 极小化场景，但风险等级"中-高"不建议
  * 路径 B：拆 N commit（如 (1/n) impl crate first release + (i/n)
    schema 注册链 + 三层 enum atomic + (j/n) S1 wiring + IkBackend +
    error 映射 + (n/n) ik_backend_smoke + e2e；具体 N 跟拆段顺序
    Plan 阶段拍板）
  * **reviewer 拍板原则**：先 land impl crate（不引入 wire schema）
    vs 先 land schema 注册链 + ActionKind enum（不引入 backend
    runtime）vs S1 wiring 优先级 —— Plan 阶段拍板
- §6.7 不变量影响：5 条全保（**0** 降级 / **0** 升级）
- §1.5 状态机 / B3 wire 契约 / B4 frozen schema：现有字段编号 + 字段
  全保；仅 append 新 variant / 新 schema_id（详 §"严禁改动"）

---

## Acceptance Criteria

> AC 在代码之前定义。沿 round 1 / 候选 1 同款。AC 数量预估 ~10-12
> （基础 AC + Quality Gate）；具体 reviewer round 1 拍板。**所有 AC
> 必须可验证**（命令 / 文件 diff / 测试断言 / 编译期锁），**不**含
> "实现 IK" 等不可判定项。

### AC-1: `crates/robot-kinematics-xlerobot/` workspace member append + Cargo.toml 合规

- **前置条件**：round 2 实施 commit 之后
- **验证**：
  * `Cargo.toml` workspace `members` 数组含 `"crates/robot-kinematics
    -xlerobot"`（按字母序在 `crates/robot-kinematics` 之后）
  * `Cargo.toml:25` "未来 crate" 注释段 `robot-kinematics-xlerobot`
    行已删除
  * `crates/robot-kinematics-xlerobot/Cargo.toml` `[dependencies]` 段
    含 `robot-kinematics = { workspace = true }`（唯一**内部** dep）
  * `[dependencies]` 段**不**含 `robot-core` / `robot-msg` /
    `robot-s0` / `robot-s1` / `robot-rpc` / `robot-transport` /
    `robot-adapters` 等其它 robot-system 内部 crate dep（impl crate
    顶层独立性 + §4.1 设计原则）
  * `crates/robot-kinematics-xlerobot/Cargo.toml` 含 `[lints]` 段 +
    `workspace = true`（继承 workspace lint 基线；既有 workspace
    member 全部显式继承；漏写 = 阻断）
  * `cargo build -p robot-kinematics-xlerobot --release` 通过
  * `cargo tree -p robot-kinematics-xlerobot --edges normal,no-proc-
    macro` 直接 dep 含 `robot-kinematics`；**是否**含 `nalgebra` /
    `nalgebra-glm` 等外部数值库待 Q-IK2 拍板；本 AC **不**预决
- **验证方式**：`cargo build -p robot-kinematics-xlerobot --release`
  + `cargo tree -p robot-kinematics-xlerobot --edges normal,no-proc-
  macro`

### AC-2: `XLeRobotArmIkSolver` impl `ArmIkSolver` for round 1 trait（**0** round 1 改动）

- **前置条件**：AC-1 land 之后
- **验证**：
  * `crates/robot-kinematics-xlerobot/src/lib.rs` 含 `pub struct
    XLeRobotArmIkSolver { ... }` + `impl ArmIkSolver for
    XLeRobotArmIkSolver`
  * `info()` 返 `&KinematicInfo`（5 关节：`shoulder_pan` /
    `shoulder_lift` / `elbow_flex` / `wrist_flex` / `wrist_roll` 或
    XLeRobot URDF / hardware adapter 一致顺序；具体策略待 Q-IK10 关
    节名一致性校验路径拍板）
  * `solve(target, seed, deadline_ns)` 移植 dimos `arm_ik.py`
    5-DOF 闭式解（`_planar_ik` + 几何法 + `wrist_roll` 平凡映射；
    dimos `arm_ik.py:103` 注释 "算法来自 XLeRobot SO101Kinematics.
    inverse_kinematics"，dimos 端是上游 `SO101Robot.py` 算法的中文 doc
    完整化移植；本 round 2 Rust 移植**首选 dimos `arm_ik.py`** 作直
    接源（中文 doc + 关节名映射已贴 XLeRobot 实际硬件），如发现行为
    跟上游 `SO101Robot.py` 偏离 → 走 Plan-Execute diff 拍板对齐方向；
    详 CodeMap §2.1）
  * `git diff <round-2-base>..HEAD --name-only` **不**含 `crates/
    robot-kinematics/src/lib.rs` 任一行（round 1 trait crate **0**
    改动；hard boundary）
  * `cargo test -p robot-kinematics` 既有 4 lib unit 全过（round 1
    锁测沿用）
- **验证方式**：`git diff --name-only` 文件清单审计 + `cargo test
  -p robot-kinematics --lib` + 编译期 trait impl 检查（如 trait 不
  匹配编译 fail）

### AC-3: `XLeRobotArmIkSolver` lib unit + clamp 行为显式锚句（reviewer 审查重点 #2）

- **前置条件**：AC-2 land 之后
- **验证**：
  * `crates/robot-kinematics-xlerobot/src/lib.rs` 含 lib unit module
    + 至少 N 个 `#[test]`（具体 N 待 Plan 拍板；CodeMap 倾向 ≥ 5
    含 FK round-trip / 工作空间边界 / 关节限位 / yaw 处理 / clamp 行
    为）
  * `XLeRobotArmIkSolver` impl crate **显式**声明 **round 1 AC-6**
    clamp 行为路径（路径 A `Ok(IkSolution { joints_rad: clamped })`
    vs 路径 B `Err(IkError::OutOfReach { distance_m })`；具体走哪条
    路径待 **Q-IK16** 拍板；本 AC **不**预决。yaw / 非规范化 quat 等
    invalid input 路径独立走 Q-IK6 + AC-8 错误映射，跟 clamp /
    out-of-reach 边界不重叠。**注**："AC-6" 指 round 1 Spec
    [`team-swe/specs/v0_4_robot_kinematics.md`](v0_4_robot_kinematics.md)
    的 AC-6 "trait 层不预钉 XLeRobot clamp 行为"；**不是**本 Spec
    AC-6（S1 solver mount point）—— 避免命名冲突）
  * 选定路径在 **3 处双写防漂移**（沿候选 2 round 1 AC-6 同款）：
    (1) `XLeRobotArmIkSolver` impl crate doc; (2) `crates/robot-
    kinematics-xlerobot/Cargo.toml [package].description` 锚句; (3)
    Spec 本节锚句
  * 至少 1 个 lib unit 覆盖 "out-of-reach 行为"（断言选定路径生效；
    如选 A → 断言 `Ok(clamped_value within workspace boundary)`；如
    选 B → 断言 `Err(IkError::OutOfReach { distance_m })`）
- **验证方式**：`cargo test -p robot-kinematics-xlerobot --lib` + 文
  件 diff 审计 3 处双写

### AC-4: `IkPayload` fbs body + 注册链 6 步 atomic + `SCHEMA_IK_PAYLOAD = 0x0000_040C`

- **前置条件**：round 2 实施 commit 之后
- **验证**：
  * `crates/robot-msg/schemas/ik_payload.fbs` 含 `namespace robot.
    msg;` + `table IkPayload { ... }` + `root_type IkPayload;`（字段
    集 minimal vs 扩展集待 Q-IK4 拍板；本 AC **不**预决）
  * `crates/robot-msg/build.rs:57` `current_schemas: [&str; 14]`
    （13 → 14）+ 数组含 `"ik_payload.fbs"`
  * `crates/robot-msg/src/schema_id.rs` 含 `pub const SCHEMA_IK_
    PAYLOAD: u32 = 0x0000_040C;` + `SCHEMA_REGISTRY` 末尾 entry
    `(SCHEMA_IK_PAYLOAD, "robot.msg.IkPayload")`；**append 后
    `SCHEMA_REGISTRY` 共 22 条**（当前 21 条 → +1）
  * `crates/robot-msg/src/markers.rs` 含 `pub struct IkPayload;` +
    `impl BodyMarker for IkPayload`（4 const 字段对齐 +
    `type Root<'a> = msg::IkPayload<'a>`）
  * `crates/robot-msg/src/size_tier.rs::default_tier_of` match arm
    含 `SCHEMA_IK_PAYLOAD => SizeTier::<待 Q-IK5 拍板>`（CodeMap
    倾向 `Control`，本 AC **不**预决）
  * `cargo run -p robot-msg --bin schema_lint` 过（含
    `SCHEMA_IK_PAYLOAD` 唯一 + 升序 + 未与 deprecated 冲突）
  * `cargo test -p robot-msg` 全过（schema_lint + default_tier_of
    穷举 + `SCHEMA_REGISTRY` 长度断言自动覆盖）
  * **atomic**：注册链 6 步**必须**在同一 commit 内 land；任一步缺
    失 = build / lint fail（沿候选 1 reviewer Plan round 1 阻断 1
    锁定模式）
- **验证方式**：`cargo run -p robot-msg --bin schema_lint` +
  `cargo test -p robot-msg` + 文件 diff 审计 6 步

### AC-5: 三层 `ActionKind::Ik` enum append + 下游映射 atomic + variant count 锁测扩

- **前置条件**：round 2 实施 commit 之后
- **验证**：
  * `crates/robot-rpc/proto/robot_control.proto` `ActionKindProto`
    enum 含 `ACTION_KIND_IK = 4`（prost 生成名 `ActionKindProto::
    ActionKindIk`，沿 `ActionKindScripted` CamelCase + enum prefix
    同款）
  * `crates/robot-msg/schemas/action_request.fbs` `ActionKindFb`
    enum 含 `Ik = 4`（不动现有 variant 编号）
  * `crates/robot-s1/src/action.rs` `ActionKind` enum 含 `Ik`
    variant
  * `crates/robot-rpc/src/task_handlers.rs::action_kind_proto_to_fb`
    match arm 含 `ActionKindProto::ActionKindIk => ActionKindFb::Ik`；
    exhaustive 不留 `_` 兜底；现有 `EXPECTED_VARIANT_COUNT` 锁测
    （`task_handlers.rs:1546` 现 = 3）→ 4
  * `crates/robot-s1/src/rr_server/handler.rs` `ActionKindFb →
    ActionKind` match arm 含 `ActionKindFb::Ik => ActionKind::Ik`；
    exhaustive；现有 variant count 锁测同步扩
  * `crates/robot-s1/src/action.rs` 现有 `action_kind_three_v04_
    variants` 测（候选 1 land）更新断言：variant 集 = `{DwellMs,
    HoldPose, Scripted, Ik}` 四项 + HashSet 长度断言 4；测名同步改
    成 `action_kind_four_v04_variants`（沿候选 1 (1/3) 同款重命名
    模式）
  * **atomic**：三层 enum append + 下游映射 5 处（dispatch + variant
    count 锁测 + 测名）**必须**同 commit 内 land；任一缺失 = 编译
    fail（沿候选 1 reviewer Plan round 1 阻断 1 锁定模式）
- **验证方式**：`cargo test --workspace --tests --features "robot-
  rpc/client" -- --test-threads=1` 全过 + 文件 diff 审计 atomic

### AC-6: S1 solver mount point + ownership 形态（**按 Q-IK3 拍板路径落地**）

- **前置条件**：AC-2 land 之后
- **验证**：本 AC **不**预决 solver mount 形态；具体验证项**条件化**
  按 Q-IK3 拍板路径分支：
  * **如 Q-IK3 拍 (a) `Coordinator::solver: Option<Box<dyn ArmIkSolver>>`**：
    - `crates/robot-s1/src/coordinator.rs::Coordinator` struct 含
      新字段 `solver: Option<Box<dyn ArmIkSolver>>`
    - `Coordinator::new()` 签名扩 solver 参数
    - `crates/robot-s1/src/action.rs::ActionContext` struct 含新字
      段 `pub solver: Option<&'a dyn ArmIkSolver>`（借用 + lifetime
      `'a`；不持权）
    - `run_action_tick_loop` / coordinator tick path 注入 `solver`
      到 `ActionContext`
  * **如 Q-IK3 拍 (b) `Coordinator::solver: Option<Arc<dyn ArmIkSolver>>`**：
    - 同 (a) 但 `Coordinator::solver` 类型切 `Arc`；`ActionContext::
      solver` borrow 形态不变
    - Spec round amend 必须附带"跨线程共享"justification 锚句
  * **如 Q-IK3 拍 (c) `Coordinator::solvers: HashMap<ArmId, ...>`**：
    - **违反** §"严禁改动" "不开多臂 dispatch" hard boundary；
      reviewer 必须明确解锁 + 同步改 hard boundary 列表
    - `ActionContext` 字段形态由 Spec round amend 重新拍板
  * **如 Q-IK3 拍 (d) `IkBackend` 自持 static**：
    - `Coordinator` / `ActionContext` **0** 改动（不持 solver）
    - `IkBackend` 内部 `static SOLVER: Lazy<...>` 或同款形态
    - Spec round amend 必须附带 round 1 Q4(X) "运行时多 instance" 拍
      板精神跟 (d) static 路径的兼容性说明
  * Startup fail-fast 路径行为（待 Q-IK10 拍板：(a) feature off =
    `NoBackendForKind` / (b) lazy `Rejected` / (c) `Coordinator::
    new()` fail-fast）；本 AC **不**预决，仅断言"行为跟 Q-IK10 拍
    板路径一致"
  * 任一拍板路径必须保证 `IkBackend::start()` 内能拿到 `ArmIkSolver`
    引用调 `solver.solve(...)`（实施期编译期验证 = 类型 check 自动锁）
- **验证方式**：`cargo test -p robot-s1 --lib` + 编译期类型 check
  + `cargo build --workspace --release`

### AC-7: `IkBackend` 第三个真 action backend + `source_task_id == ctx.command_seq` 严格单调（reviewer 审查重点 #1）

- **前置条件**：AC-5 + AC-6 land 之后
- **验证**：
  * `crates/robot-s1/src/action_backend/ik.rs` 含 `pub struct
    IkBackend;` + `inventory::submit! { &IkBackend as &dyn
    ActionBackend }`（沿 `HoldPoseBackend` / `ScriptedBackend` 同
    款 static 注册模板）
  * `impl ActionBackend for IkBackend`：`name() = "ik"` +
    `supported_kinds() = &[ActionKind::Ik]` + `supported_schemas()
    = &[SCHEMA_IK_PAYLOAD]`
  * `IkBackend::start()`：
    - decode `IkPayload` fbs（payload_schema_id 校验 +
      `flatbuffers::root::<IkPayload>(payload)` verify）
    - 取 solver 引用（**按 Q-IK3 拍板路径**：(a)/(b)/(c) →
      `ctx.solver.ok_or(ActionStartError::Rejected("no solver
      wired"))?`；(d) → `IkBackend::SOLVER` static 直接拿）
    - 调 `solver.solve(target, seed, deadline_ns)` 解 IK
    - `IkError` → `ActionStartError` / `ActionFailure` 映射（具
      体映射边界待 Q-IK8 拍板；本 AC **不**预决）
    - 成功 → 返 `Box<IkInstance>` 持解出的 `joints_rad`
  * `IkInstance::tick()` 输出 `JointCommand` 时**必须**调
    `build_hold_command(ctx.config, ctx.layout_id, &target_joints,
    ctx.command_seq, ctx.clock.now().0)` 或 `build_joint_command(
    ..., source_task_id = ctx.command_seq, ...)` 既有路径（详
    `coordinator.rs:362-378` 签名）；**严禁** backend 私有 counter
    / wall clock / 任何绕过 `ctx.command_seq` 的路径
  * lib unit 含 `source_task_id_eq_ctx_command_seq` bit-equivalent
    锁测：构造 `ActionContext { command_seq: N }` → tick →
    `JointCommand.source_task_id == N`（沿候选 1 (2/3) `tick_
    segment_returns_command_with_source_task_id_eq_ctx_command_
    seq` 同款 bit-equivalent 锁）
  * single-shot vs trajectory-like 路径（**Q-IK17** 拍板）拍板后**显
    式**实现 + lib unit 覆盖（具体行为边界待 Q-IK17 拍板；本 AC
    **不**预决）
  * cancel 收敛模式：single-shot 路径 `cancel()` 几乎不可能触发；
    trajectory-like 路径沿 `HoldPoseBackend` / `ScriptedBackend`
    同款 `cancelled: bool` flag + 下次 tick 返
    `PartialSuccess { "cancelled" }`（具体细节绑 **Q-IK17** 输出模
    式拍板路径；跟 Q-IK4 字段集 / Q-IK5 size tier 无关）
- **验证方式**：`cargo test -p robot-s1 --lib` 含 bit-equivalent
  锁测 + 编译期 trait impl 检查 + cross-process e2e（AC-10）实测
  覆盖

### AC-8: `IkError` 错误映射边界（reviewer 审查重点 #2）

- **前置条件**：AC-7 land 之后
- **验证**：
  * `IkError` 5 variant → `ActionStartError` / `ActionFailure` 映
    射边界**完整覆盖**（5 variant 全有归属；不留 `_ => unreachable!()`
    / 类似兜底）；具体映射表待 Q-IK8 拍板（本 AC **不**预决）
  * 映射逻辑**集中**在 `crates/robot-s1/src/action_backend/ik.rs`
    或 `crates/robot-s1/src/action.rs`（具体位置待 Plan 拍板；不
    分散到多文件）
  * `ActionFailureKindFb` **0** 改动（保 5 variant + Unknown frozen
    schema 边界；hard boundary）；`IkError::OutOfReach` /
    `IkError::SingularityTooClose` / `IkError::NoSolution` 等映射
    到现有 `ActionFailureKindFb::PreconditionFailed` 或类似（具体
    拍板待 Q-IK8 + Q-IK9）
  * lib unit 覆盖 5 variant 全映射路径（每 variant 至少 1 个
    `#[test]`；assert 终态 `ActionStartError` / `ActionFailure`
    类型 + `ActionRpcStatus` 跨进程映射结果）
  * **3 处双写防漂移**（沿候选 2 round 1 AC-6 同款）：
    (1) `IkBackend::start()` doc 注释 + 映射代码集中；(2) Spec 本
    节锚句；(3) `ik_backend_smoke --phase invalid_input` /
    `--phase unreachable` 端到端验证错码（详 AC-10）
- **验证方式**：`cargo test -p robot-s1 --lib` 5 variant 全映射
  + `cargo test --workspace --tests --features "robot-rpc/client"
  -- --test-threads=1` workspace 不退化 + cross-process e2e
  实测错码

### AC-9: S1 feature flag + backend dispatch + optional gateway payload cap

- **前置条件**：AC-5 + AC-7 land 之后
- **验证**：
  * `crates/robot-s1/Cargo.toml` `[features]` 段含 IK 相关 feature
    （粒度待 Q-IK11 拍板）；如选 default-on，`default = [..., "<ik
    feature name>", ...]` 含
  * `crates/robot-s1/src/action_backend/mod.rs` 含
    `#[cfg(feature = "<ik feature name>")] pub mod ik;`
  * gateway 层 IK payload byte cap（如 **Q-IK5 size tier** 或 Plan
    阶段 payload cap 子项拍板需要）—— 沿候选 1 `MAX_SCRIPTED_PAYLOAD_
    BYTES` + `CONTROL_TIER_MAX_BYTES` 单源绑定 `SizeTier::Control.
    max_bytes()` 同款模板；具体值（如 `MAX_IK_PAYLOAD_BYTES = ?`）
    待 Plan 拍板；本 AC **不**预决是否落 round 2。**注**：跟 Q-IK4
    字段集语义解耦 —— Q-IK4 锁字段类型 / 名称 / 数量；byte cap 是
    payload **容量边界**（size_tier + 可选 precheck），属 Q-IK5 容量
    层 / Plan payload cap 子项
  * lib unit 含 dispatch 单测（`BackendRegistry::dispatch` 返
    `&IkBackend` 时验证 `name == "ik"` + `supported_kinds ==
    [Ik]` + `supported_schemas == [SCHEMA_IK_PAYLOAD]`）
- **验证方式**：`cargo build --workspace --release` 双路（feature
  on / off）+ `cargo test -p robot-s1` + `cargo test -p robot-rpc`

### AC-10: `ik_backend_smoke` cross-process e2e（phase 集合按 Q-IK13 拍板）

- **前置条件**：AC-7 + AC-8 + AC-9 land 之后
- **验证**：
  * `crates/robot-tests/Cargo.toml` 含 `[[bin]] name = "ik_backend_
    smoke"` + `required-features = ["ik-backend-smoke"]` + `ik-
    backend-smoke` feature 段（沿 `scripted-backend-smoke` 同款）
  * `crates/robot-tests/src/bin/ik_backend_smoke.rs` 含 CLI args
    `--bin-dir / --robot-id-prefix / --hardware-id（required） /
    --config（required）/ --phase / --output-csv`（沿候选 1 / 3
    同款）
  * phase 集合待 Q-IK13 拍板（CodeMap 倾向 5：`selftest /
    happy_single_target / source_task_id_monotonic / unreachable /
    invalid_input / all`；本 AC **不**预决具体集合）
  * `configs/ik_backend_smoke.toml` 含 XLeRobot IK profile +
    `[s1.profile.backends] enabled = ["ik", ...]`
  * `cargo build --release -p robot-tests --bin ik_backend_smoke
    --features ik-backend-smoke` 过
  * `./target/release/ik_backend_smoke --phase all --hardware-id
    smoke_arm --config configs/ik_backend_smoke.toml` exit code 0；
    所有 phase PASS（具体 PASS 锚句对齐 Q-IK13 拍板）
  * `source_task_id_monotonic` phase 端到端验证 B3 §3.7 单调契约
    （`JointCommand.source_task_id` 跨多次 IK action 严格单调递增）
  * `unreachable` phase 端到端验证 AC-3 clamp 行为路径（assert 选
    定路径 A `Completed(Success)` 或 B `Failed(PreconditionFailed)`
    或 Q-IK6 yaw 处理路径）
  * `invalid_input` phase 端到端验证 AC-8 错误映射（NaN
    translation / 非规范化 quat / seed.len() != joint_count →
    `Failed(PreconditionFailed)` 或 gateway `FailedPrecondition`）
- **验证方式**：cross-process binary `--phase all` exit 0 + 各
  phase 个别可跑 + smoke binary 内 `#[test]` 模块（如有）`cargo
  test --bin ik_backend_smoke` 过

### AC-11: 零 round 1 trait crate 改动 + 文件清单 hard boundary（reviewer 审查重点 #4）

- **前置条件**：round 2 实施 commit 之后
- **验证**：
  * `git diff <round-2-base>..HEAD --name-only` 输出**不**含：
    - `crates/robot-kinematics/src/lib.rs` 任一行（round 1 trait
      crate first release **0** 改动；hard boundary）
    - `crates/robot-kinematics/Cargo.toml [dependencies]` 段（同上）
    - `docs/00_design.md` 正文
    - `deny.toml`
  * `git diff <round-2-base>..HEAD --name-only` 输出**仅**含上述
    §"必改文件" 表所列文件（+ Cargo.lock 自动 + 可选 README）
  * `cargo build --workspace --release` 通过（验证既有 12 crate
    + round 2 新 crate / 新 backend 不退化）
  * `cargo test --workspace --tests --features "robot-rpc/client" --
    --test-threads=1` 既有 963+ PASS（round 1 land 后基线）+ round 2
    新加 lib unit 全过；**0** 回归
- **验证方式**：`git diff --name-only` 文件清单审计 + workspace
  build/test 不退化锁测

### AC-Q: Quality Gate（不进 AC，但是 PASS 条件）

- `cargo fmt --all` clean
- `cargo clippy --workspace --all-targets -- -D warnings` 双路 clean
  （含 / 不含 `robot-rpc/client` feature）+ 含 / 不含 round 2 新
  feature `<ik feature name>` / `ik-backend-smoke` clean
- `cargo test --workspace --tests --features "robot-rpc/client" --
  --test-threads=1` 既有 963+ PASS（round 1 land 后基线）+ round 2
  新加 lib unit 全过
- `cargo test -p robot-kinematics --lib`：round 1 既有 4 lib unit
  **0** 回归（trait crate 不改）
- `cargo test -p robot-kinematics-xlerobot --lib`：新 lib unit 全过
- `cargo test -p robot-s1 --lib`：含 source_task_id bit-equivalent
  锁测 + IkError 映射 5 variant 全过
- `cargo run -p robot-msg --bin schema_lint` 过（含 `SCHEMA_IK_PAYLOAD
  = 0x040C` 唯一 + 升序 + 未与 deprecated 冲突）
- `cargo doc -p robot-kinematics-xlerobot --no-deps` 过（无 warning；
  含 clamp 行为锚句双写）
- `crash_restart_smoke --iterations 5`（real-s1 路径）**5/5 PASS**
  是**硬 gate**（沿候选 1 + 2 round 1 + 3 同款回归门槛）
- `crash_restart_smoke --use-mock-s1 --iterations 5`（mock-s1 路径）
  按既有 baseline 记录为 informational only（沿候选 2 round 1 实测
  5/5 PASS / candidate 1 (1/3) 0/5 baseline 记录；**不**升级为新
  硬 gate）
- **重型 smoke 升回 (a) full `--phase all` 硬 gate**（round 1 拍 (b)
  compile gate + selftest 是零 runtime 特例；round 2 含 backend
  impl + S1 wiring + cross-process e2e，必须升回）—— 各 binary
  PASS 语义按自身 phase 集合实际语义书写：
  * `pubsub_cleanup_smoke --phase all`（候选 3 e2e）是**硬 gate**：
    `exit code 0` + AC-A1/A2/A3 实测 PASS + 候选 3 AC-C1/C2/C3 三
    条件性 trigger 全 `false`（沿候选 3 summary §3 锚句；本轮改
    动**不**应触发任一条件性 AC）+ `events_registry_len final == 0`
  * `scripted_backend_smoke --phase all`（候选 1 e2e）是**硬 gate**：
    `exit code 0` + 各 phase（`selftest` / `happy_multi_waypoint` /
    `source_task_id_monotonic` / `cancel_mid` / `payload_boundary`）
    PASS 锚句对齐候选 1 summary §4.1；本轮改动**不**应触发候选 1
    AC-4/5/6/7 任一回归
  * `ik_backend_smoke --phase all`（本 round 2 新增 e2e）是**硬
    gate**：`exit code 0` + **按 Q-IK13 拍板的 phase 集合全部 PASS**
    （phase 数 / 具体集合 reviewer Q-IK13 拍板后 Spec round amend 锁
    定；本 AC-Q 初版**不**预决 phase 数）
- `cargo deny check` 状态（沿候选 2 round 1 §P.4 diff #1 同款措辞）：
  **未新增 violation**；overall `licenses FAILED` 是**既有 baseline**
  （`assert_no_alloc BSD-1-Clause` + `serialport MPL-2.0` +
  `unescaper` 标识符 warning；全部跟 round 2 改动无关）；follow-up
  独立 commit / PR 处理；**严禁**写 "`cargo deny check` 过"
- §6.7 5 条不变量真宣称保持（**0** 降级 / **0** 升级 / **0** 偏移）

---

## Reviewer round 1 拍板表 (Q-IK1..Q-IK13 + Q-IK16..Q-IK17 全 pending；CodeMap Q-IK14 / Q-IK15 下沉到 AC-Q / hard boundary)

> **Q 编号对齐 CodeMap 稳定**（reviewer Spec round 1 amend 拍板）：
> - CodeMap `team-swe/codemap/v0_4_robot_kinematics_round2.md` §4
>   Q-IK1..Q-IK15 编号**保持不动**；引用一致性沿用
> - CodeMap **Q-IK14** = `cargo deny check` baseline 处理（沿候选 2
>   round 1 §P.4 diff #1）→ **下沉到 §AC-Q "**未新增 violation；
>   overall fail 是既有 baseline**" 措辞 + §"严禁改动" `deny.toml`
>   hard boundary；**不**作 reviewer 拍板 Q
> - CodeMap **Q-IK15** = round 2 是否触 §6.7 任一不变量 → **下沉到
>   §"严禁改动" "§6.7 5 不变量任一条降级 / 升级 — **0** 触" hard
>   boundary 锚句；**不**作 reviewer 拍板 Q
> - Spec round 1 **新增** Q-IK16（clamp 行为路径）+ Q-IK17（`IkInstance::
>   tick` 输出模式）覆盖 round 1 spec AC-6 在 round 2 落地时的实施期
>   决策点；编号直接跳到 16/17 避免跟 CodeMap Q-IK14/Q-IK15 冲突

> **本 Spec 初版 Q-IK1..Q-IK13 + Q-IK16..Q-IK17 全部保留为待拍板项**（reviewer 硬要
> 求："Q-IK1..Q-IK13 + Q-IK16..Q-IK17 先全部保留为待拍板项，不要在 Spec 初版里把
> CodeMap 倾向写成已 ACK"）。CodeMap §4 "默认倾向（Plan 阶段确认）"
> 标记**仅供参考**；reviewer Spec round 1+2+... amend 拍板后才锁。

reviewer 在 CodeMap commit `a42ef73` 之后 round 2 Spec round 1 阶段
拍板 Q-IK1..Q-IK13 + Q-IK16..Q-IK17。下表是 round 1 实测 Q 列表 + CodeMap §4 候选方
向参考；具体选项 reviewer 拍板：

| Q | 内容 | 候选选项 | 落 AC / 影响范围 |
|---|---|---|---|
| **Q-IK1** | `robot-kinematics-xlerobot` workspace dep 入口形态 | (a) 仅 impl crate workspace dep 入口；(b) 双 `workspace = true` 入口（trait crate + impl crate） | AC-1 |
| **Q-IK2** | impl crate 是否启用 `nalgebra` 可选 feature | (a) 否，沿 dimos 闭式解 + round 1 零依赖原则；(b) 是，optional feature gate | AC-1 + 风险等级 |
| **Q-IK3** | Solver ownership 形态 | (a) `Coordinator::solver: Option<Box<dyn ArmIkSolver>>` + `ActionContext::solver: Option<&'a dyn ArmIkSolver>`（reviewer 入口锚定默认）；(b) `Arc<dyn>` 替 `Box<dyn>`（需"跨线程共享"justification）；(c) `HashMap<ArmId, ...>` 多臂（违反 hard boundary "不开多臂 dispatch"，需 reviewer 明确解锁）；(d) `IkBackend` 自持 static | AC-6 + §"严禁改动" |
| **Q-IK4** | `IkPayload` 字段集 | (a) 最小集（target_xyz + target_quat_xyzw + seed_joints_rad + frame）；(b) 扩展集（+ tolerance / arm_id / priority / ...） | AC-4 + AC-9 子问题 |
| **Q-IK5** | `SCHEMA_IK_PAYLOAD` size_tier | (a) `Control` (8 KiB)；(b) `SensorSmall` (1 MiB)；(c) 其它 | AC-4 |
| **Q-IK6** | 5-DOF yaw 处理策略 | (a) yaw != 0 返 `Err(IkError::InvalidInput { reason })`；(b) ignore | AC-3 + AC-8 + AC-10 (`invalid_input` phase) |
| **Q-IK7** | `IkPayload.frame: string` 字段是否纳入 round 2 | (a) 纳入（占位字符串；实际 frame 变换推未来）；(b) 不纳入 | AC-4 + §"严禁改动" |
| **Q-IK8** | `IkError` 5 variant → `ActionStartError` (start 期) vs `ActionFailure` (tick 期) 映射边界 | (a) 全 `ActionStartError`（single-shot + 闭式解全 start 期）；(b) 拆 start vs tick；(c) 其它 | AC-7 + AC-8 |
| **Q-IK9** | 是否扩 `ActionFailureKindFb` 加第 6 variant `IkSolverError` | (a) 否，沿现有 5 variant + `PreconditionFailed` 兜底（保 frozen schema）；(b) 是，扩 variant（违反 hard boundary，需 reviewer 明确解锁 + 独立 reviewer round amend `action_status.fbs`） | AC-8 + §"严禁改动" |
| **Q-IK10** | Solver 缺失时 startup fail-fast 策略 | (a) feature off = no register = `NoBackendForKind`；(b) feature on + solver = None = lazy `Rejected`；(c) `Coordinator::new()` fail-fast `Err(InitError::SolverMissing)` | AC-6 |
| **Q-IK11** | feature flag 粒度 | (a) `backend-ik` 单一 default 进 + 默认 wired XLeRobot solver；(b) `backend-ik` + `kinematics-xlerobot` 双 feature 解耦 backend 启用 vs solver 选型；(c) `kinematics-<robot>` per-robot 多 solver | AC-9 + §"必改文件" `robot-s1/Cargo.toml` |
| **Q-IK12** | smoke helper 复用策略 | (a) inline 复制（沿候选 1 Q5(a)）；(b) 抽公共 crate（候选 1 拒过的路径 B） | AC-10 + §"必改文件" |
| **Q-IK13** | `ik_backend_smoke` phase 数量 + 具体集合 | (a) 5 phase（CodeMap 倾向）；(b) 4 phase（合并 unreachable + invalid_input）；(c) 其它 | AC-10 + AC-Q |
| **Q-IK16** | `XLeRobotArmIkSolver` clamp 行为路径（**Spec round 1 新增**；CodeMap Q-IK14 = `cargo deny check` baseline 处理已下沉到 AC-Q 措辞，**不**作 reviewer 拍板 Q）| (a) `Ok(IkSolution { joints_rad: clamped })` + 外部 informational metric（沿 dimos 现有行为）；(b) `Err(IkError::OutOfReach { distance_m })`（trait 层显式失败）| AC-3 + AC-10 (`unreachable` phase) |
| **Q-IK17** | `IkInstance::tick` 输出模式（**Spec round 1 新增**；CodeMap Q-IK15 = round 2 是否触 §6.7 任一不变量已下沉到 §"严禁改动" hard boundary "**0** 触" 锚句，**不**作 reviewer 拍板 Q）| (a) single-shot（一次 IK + 一帧 publish + `Completed(Success)`）；(b) trajectory-like（IK 解 + 多 tick 插值 + dwell + cancel 路径，沿 `ScriptedBackend` 模板） | AC-7 + AC-Q |

> Q-IK1..Q-IK13 + Q-IK16..Q-IK17 reviewer ACK 后，Spec round 2+ 内**不**重新拍板任一
> Q；偏离需走 `team-swe/specs/v0_4_robot_kinematics_round2.md` §P.4
> Plan-Execute diff 模板（沿候选 1 / 2 round 1 同款 appendix-only
> 登记机制）。

---

## reviewer 审查重点 (round 2 实施期硬 gate)

reviewer 给 Q-IK1..Q-IK13 + Q-IK16..Q-IK17 拍板时同时锁 4 条审查重点；本 Spec 把每条
design-in 到具体 AC + 严禁改动列表：

| # | 审查重点 | 落地 |
|---|---|---|
| 1 | `IkBackend` 输出走 `JointCommand` path 不绕 B3 §3.7 `source_task_id == ctx.command_seq` 单调契约 | AC-7 bit-equivalent 锁测 + `source_task_id_monotonic` smoke phase 端到端 + §"严禁改动" hard boundary 明文 + reviewer 审查重点 #1 |
| 2 | IK fail 映射边界一致性（`IkError` 5 variant 全有归属 + 错码跨 3 层 lib/S1/gateway 一致 + 不扩 frozen `ActionFailureKindFb`） | AC-8 5 variant 全映射 lib unit + `invalid_input` / `unreachable` smoke phase 端到端 + AC-3 clamp 行为 3 处双写 + §"严禁改动" `ActionFailureKindFb` hard boundary |
| 3 | `IkPayload` wire schema first release 稳定性（字段集 Q-IK4 拍板后即作 wire schema first release；未来扩字段走独立 reviewer round） | AC-4 schema 注册链 6 步 atomic + `schema_lint` 升序自动校验 + §"严禁改动" `B4 frozen wire schema` hard boundary |
| 4 | 零 round 1 trait crate 改动 + 零 docs/00_design.md 改动 + 零 deny.toml 改动 | AC-11 `git diff --name-only` 文件清单审计 + §"严禁改动" 14+ 路径 hard boundary + `cargo build --workspace` 既有 12 crate 不退化锁测 |

---

## 不在范围（候选 2 round 2 边界）

按 v0.4_entry §3.3 + CodeMap §5 + 上面 §"严禁改动" + Q-IK1..Q-IK13 + Q-IK16..Q-IK17
拍板：

- **`BaseKinematics` trait + `BaseKinematicInfo`** —— round 1 Q5(b)
  ACK 推到 round 2 但 round 2 **仍不开**；推到 round 3 / 候选 4 跟
  XLeRobot 三轮全向底盘运动学一起；本 round 2 `robot-kinematics`
  trait crate **0** 加新类型 / trait
- **`robot-kinematics-piper` impl crate** —— v0.6+ 结构异构验证；本
  round 2 workspace `members` **不**含；`Cargo.toml:27` 注释段保留
- **多臂 dispatch** —— round 1 Q4(X) ACK "多 solver instance" 推未
  来 reviewer round；round 2 单臂先落；**不**加 `Coordinator::solvers:
  HashMap<ArmId, ...>` / `IkPayload.arm_id` 字段 / per-arm `ArmId`
  类型；如 Q-IK3 拍板 (c) `HashMap` 路径 → reviewer 必须明确 ACK
  解锁本边界
- **URDF 加载 / vendor URDF parser** —— round 2 闭式硬编码（dimos
  移植）；vendor URDF / `kinpy` / `urdfpy` 等 IK / 矩阵 / URDF 第三
  方依赖**不**进 round 2；推未来 reviewer round
- **`FrameName` enum / `TransformTree` / 实际 frame 变换** —— round
  2 即使 Q-IK7 拍板 (a) 纳入 `IkPayload.frame: string` 字段，也仅
  作占位字符串；实际 frame 变换路径推未来 reviewer round
- **frozen `ActionFailureKindFb` variant 扩展** —— round 2 保 5
  variant + Unknown；如 Q-IK9 拍板 (b) 扩 variant → reviewer 必须
  明确解锁本边界 + 走独立 reviewer round amend `action_status.fbs`
- **`solve_delta(current_angles)` / FK / Jacobian** trait 方法扩
  展 —— round 1 Q1(A) 最小 API 锁定；round 2 trait 仅 `solve()` +
  `info()`；如 round 2 实施期实测必须扩 → 走独立 reviewer round
  amend round 1
- **真硬件回归 / Orin 真机 IK 端到端** —— round 2 land 后单独安排
  （沿 v0.3 B 真硬件回归节奏）
- **`deny.toml` baseline 修复**（候选 2 round 1 §P.4 diff #1
  follow-up）—— 独立 commit / PR；**不**捆绑 round 2
- **真 auth / mTLS / OIDC / rate limiting / circuit breaker** ——
  v0.5+
- **§6.7 5 不变量任一条降级 / 升级** —— **0** 触；round 2 land 后
  预期仍 5/5
- **`docs/00_design.md` 正文偏离** —— **0** 触；round 2 schema
  append + ActionKind append + impl crate 落地都是 §4.1 预写文件结
  构自然外推，**不**走 `docs/CHANGELOG.md` 偏离登记
- **候选 1 / 候选 3 关键契约改动** —— 候选 1 `MAX_SCRIPTED_PAYLOAD_
  BYTES` / `CONTROL_TIER_MAX_BYTES` 单源绑定 / Dwell `start_ns`
  anchor / cancel 两层分离 + 候选 3 `EventsDrainRegistry::reap_
  finished` / shutdown step 4 顺序 / `Box::leak(node)` residual 全
  不触

---

## 跟其它 milestone handoff / spec 的链接

- [`team-swe/codemap/v0_4_robot_kinematics_round2.md`](../codemap/v0_4_robot_kinematics_round2.md)
  完整 Pre-Research CodeMap（8 段 + Q-IK1..Q-IK13 + Q-IK16..Q-IK17 列表；本 Spec
  round 1 Q-IK1..Q-IK13 + Q-IK16..Q-IK17 reviewer 拍板锁定）
- [`docs/handoffs/v0.4_robot_kinematics_round1_summary.md`](../../docs/handoffs/v0.4_robot_kinematics_round1_summary.md)
  candidate 2 round 1 (trait crate first release) 完结锚点 + §3 关
  键契约 6 条（round 2 全部沿用，**0** 破）+ §6 round 2 入口 10 步
  接班 checklist + §7 §P.4 diff 5 项
- [`team-swe/specs/v0_4_robot_kinematics.md`](v0_4_robot_kinematics.md)
  round 1 Spec round 1 + Plan v1 + reviewer Plan round 2/3 amend +
  Q1-Q10 拍板表 + AC-1..AC-Q + §P.4 diff 5 项归档；round 2 Spec
  （本文件）**不**续写
- [`docs/handoffs/v0.4_scripted_backend_summary.md`](../../docs/handoffs/v0.4_scripted_backend_summary.md)
  §3 关键契约审计（source_task_id 单调 / Dwell anchor / cancel 两
  层分离 / 两道 byte gate / `ActionRpcStatus` 映射 5 条；round 2
  `IkBackend` 沿用 1+2+3+5 四条；4 不触 = IK 单 payload 不需双 byte
  gate / **如 Q-IK5 size_tier 或 Plan 阶段 payload cap 子项拍板需要**）
  + §6 候选 2 接班 + §7 Plan-Execute diff 模板
- [`docs/handoffs/v0.4_pubsub_cleanup_summary.md`](../../docs/handoffs/v0.4_pubsub_cleanup_summary.md)
  §3 关键契约（`reap_finished` by `&self` / shutdown step 4 /
  `Box::leak` residual disclosure；round 2 **0** 触但回归门槛沿用
  + `pubsub_cleanup_smoke --phase all` 硬 gate）
- [`docs/handoffs/v0.3_b3_summary.md`](../../docs/handoffs/v0.3_b3_summary.md)
  §3 `source_task_id == ctx.command_seq` reasoning chain（round 2
  `IkBackend` 不可绕开；reviewer 审查重点 #1 锁定）
- [`docs/handoffs/v0.3_b2_outline.md`](../../docs/handoffs/v0.3_b2_outline.md)
  `ActionBackend` trait / `BackendRegistry` / `ActionContext` /
  `ActionTickOutput` 原始拍板表（round 2 `IkBackend` 沿 B2 trait 边
  界 + 不解锁 enum variant / 不解锁 trait 方法签名）
- [`docs/handoffs/v0.3_b4_summary.md`](../../docs/handoffs/v0.3_b4_summary.md)
  §3 7 条契约（`ActionRpcStatus` 映射 / shutdown 顺序 / stream
  lifecycle / pubsub keep-last；round 2 全部沿用）
- `docs/00_design.md` §4.1 模块地图（`arm.rs` 预写文件结构 跟
  `robot-kinematics-xlerobot` 实施路径自然外推）+ §6.6.4 errata（IK
  是可选 backend）+ §6.7 5 条不变量全保（round 2 land 后预期仍 5/5）
  + §1.5 状态机 5 variant 不动

**外部算法源参考**（**不**进 Cargo dep；仅作 Rust 移植参考审计）：

- `/home/loosqk/repo/robot/dimos-deploy/src/xlerobot_dimos/kinematics/arm_ik.py`
  —— **直接移植源**（5-DOF 闭式 + 中文 doc 完整化 + dimos 关节名映射
  已贴 XLeRobot 实际硬件 ID 顺序）；本 round 2 Rust 移植**首选**此文
  件作直接源
- `https://github.com/Vector-Wangel/XLeRobot/blob/main/software/src/model/SO101Robot.py`
  —— **算法上游 origin**（`SO101Kinematics.inverse_kinematics` 类
  方法；dimos `arm_ik.py:103` 显式注释来源）；作算法行为对齐参考，如
  发现 dimos 端跟上游偏离走 Plan-Execute diff 拍板

---

## 后续：reviewer round 2 + Plan

Spec round 1 commit 后，reviewer 进 **Spec round 2** 复核：
- AC-1..AC-11 + AC-Q 边界审 / Q-IK1..Q-IK13 + Q-IK16..Q-IK17 拍板表是否准确落 AC /
  严禁改动 14+ 路径列表是否覆盖完整 / 审查重点 4 条 design-in 是否
  双写 / 影响范围 §"必改文件" 表是否准确
- 如有阻断 → Spec round 2 amend commit；零阻断 → ACK 进 Plan 阶段

**Plan 阶段**（沿候选 1 / 2 round 1 / 3 同款 Plan v1 + round amend
模式）：
- Plan outline draft v1 commit：commit 节奏（**Plan 阶段拍板** —
  Spec 初版**不**预决）+ P.0..P.4 段
- reviewer Plan round 1+2+... amend → Plan Approved → 进入实施
- 实施期偏离 Plan 任何决策 → 登记 §P.4 Plan-Execute diff（沿候选 1
  同款 appendix-only）

**实施期 commit 节奏**：**留给 Plan 阶段拍板**（reviewer 硬要求
"round 2 实施 commit 数不在 Spec 初版预决"）。粗估 commit 数 N ≥ 2
（impl crate first release 跟 wire schema / S1 wiring / e2e 合理拆
段；Plan 阶段精确拍板）。

**round 2 完结后**（独立 reviewer round）：
- summary 归档（`docs/handoffs/v0.4_robot_kinematics_round2_summary.md`
  新文件，沿候选 1 / round 1 同款模板）+ README anchor + CHANGELOG +
  CLAUDE.md 状态段
- §6.7 5 条不变量真宣称（预期 5/5 全保）
- 候选 2 整体完结锚点（含 round 1 + round 2 两段 commit chain）
- 下一候选触发（v0.4_entry §3.1 优先级 3 → 1 → 2 → 4 完成）

未来 reviewer round（**不**在 round 2 范围）：
- `BaseKinematics` trait + `BaseKinematicInfo` reviewer round（round
  3 候选 4 dimos 切换前置）
- 多臂 dispatch reviewer round（XLeRobot dual-arm 真上线时机）
- URDF 加载 + frame transform reviewer round（vendor URDF 选型 +
  `FrameName` enum 拍板时机）
- `robot-kinematics-piper` impl crate reviewer round（v0.6+ 结构异
  构验证）

---

## Plan

> Plan outline draft v1（reviewer ACK pending）。基于 Spec §AC +
> §"严禁改动" hard boundary + §"reviewer 审查重点" 4 条硬输入。
>
> **本 Plan 初版不改 Spec 已 ACK 的语义** —— Spec round 1 commit
> `7fd0756` 之后所有 Spec 段（What / 影响范围 / 风险等级 / 回滚 /
> AC-1..AC-11 + AC-Q / Reviewer 拍板表 / 审查重点 / 不在范围 / 链
> 接）在 Plan 阶段**只读引用**；任何 Spec 语义改动**必须**列为
> Plan round amend 显式标注。
>
> Q-IK1..Q-IK13 + Q-IK16..Q-IK17 全 pending；Plan 阶段不重新拍板
> CodeMap 已下沉的 Q-IK14（`cargo deny` baseline）/ Q-IK15（§6.7
> 触碰），二者锁在 hard boundary。

### P.0 工作基线

- 起点：`master` HEAD = `7fd0756`（Spec round 1 land）。
- 上游链：CodeMap `a42ef73` → Spec round 1 `7fd0756`（本 Plan 续写）。
- 候选 2 round 1 完结锚：`f9235e2` summary 已 push；round 1 `Arm
  IkSolver` trait crate first release（`6caf0ee`）**0** 改动是 round
  2 的 hard boundary（详 Spec §"严禁改动" #1）。
- Q 拍板硬输入：**0** —— Q-IK1..Q-IK13 + Q-IK16..Q-IK17 全 pending；
  reviewer Plan round 1+ 拍板锁定后才进实施。
- CodeMap Q-IK14 / Q-IK15 下沉项已锁 hard boundary：
  * Q-IK14 `cargo deny check` baseline → AC-Q "未新增 violation；
    overall fail = 既有 baseline" 措辞；§"严禁改动" `deny.toml`
    hard boundary；**Plan 阶段不重新拍**
  * Q-IK15 §6.7 5 不变量触碰 → §"严禁改动" "**0** 触" 锚句；
    **Plan 阶段不重新拍**
- 不开新分支（沿候选 1 / 2 round 1 / 3 模式直接 commit 到 `master`
  + 每段 reviewer ACK）。
- 重型 smoke 回归门槛：**升回 (a) full e2e 硬 gate**（round 1 拍
  (b) compile gate + selftest 是零 runtime 特例；round 2 含 backend
  impl + S1 wiring + cross-process e2e，必须升回；Spec AC-Q 已锁；
  Plan 阶段不重新拍板）。

### P.1 commit 节奏总览（候选拆段；最终 P.3 #1 拍板）

> **本 §P.1 仅列候选拆段**；reviewer Plan round 1 §P.3 拍板项 #1
> 选定后才锁。Plan 初版**不**预决 commit 数。

候选拆段（4 路径；具体 N reviewer Plan round 拍板）：

**路径 A — 单 commit (1/1) 全段 atomic**（风险等级"中-高"不建议）

```
✨ feat: v0.4 候选 2 round 2 (1/1) IK backend full path land
```

仅适用极小化场景；本 round 2 风险等级"中-高"（多 crate / 多层 wire /
hot path 接入）→ 路径 A 不建议；列出仅作 reviewer 拍板参考。

**路径 B — 拆 3 commit**

```
✨ feat: v0.4 候选 2 round 2 (1/3) robot-kinematics-xlerobot impl
        crate first release land（XLeRobotArmIkSolver + dimos
        arm_ik.py 5-DOF 闭式解 Rust 移植 + lib unit + 0 wire schema
        改动 + 0 robot-s1 改动）
✨ feat: v0.4 候选 2 round 2 (2/3) IK wire schema + 三层 ActionKind
        ::Ik + S1 IkBackend + ActionContext solver 注入 atomic land
✨ feat: v0.4 候选 2 round 2 (3/3) ik_backend_smoke binary +
        cross-process e2e + configs/ik_backend_smoke.toml
📝 docs: v0.4 候选 2 round 2 summary 归档
```

**路径 C — 拆 4 commit**（impl + schema atomic + S1 wiring + e2e
四段，沿候选 1 (1/3)/(2/3)/(3/3) 同款节奏倾向）

```
✨ feat: v0.4 候选 2 round 2 (1/4) robot-kinematics-xlerobot impl
        crate first release land
✨ feat: v0.4 候选 2 round 2 (2/4) IK schema 注册链 6 步 + 三层
        ActionKind::Ik enum append + dispatch 映射 atomic land
✨ feat: v0.4 候选 2 round 2 (3/4) S1 Coordinator solver 字段 +
        ActionContext::solver 注入 + IkBackend + 错误映射 land
✨ feat: v0.4 候选 2 round 2 (4/4) ik_backend_smoke binary +
        cross-process e2e
📝 docs: v0.4 候选 2 round 2 summary 归档
```

**路径 D — 拆 5+ commit**（如 reviewer Plan round 要求更细粒度，
如 S1 wiring 再拆为 Coordinator + ActionContext + IkBackend 三 commit）

> **拒绝 / 倾向理由 reviewer Plan round 1 拍板** —— Plan 初版**不**
> 预决；详 §P.3 拍板项 #1。

### P.2 工作流（按拆段顺序展开；具体段顺序绑 P.3 #1 拍板）

> §P.2 按"逻辑依赖序"展开（**不**等于 commit 序）：先 impl crate（不
> 引入 wire schema），后 schema 注册链 atomic，再 S1 wiring + IkBackend，
> 最后 e2e smoke。reviewer Plan round 拍板 commit 节奏后再决定哪几
> 步合 commit。

#### P.2.1 [impl crate 段] `robot-kinematics-xlerobot` first release

##### P.2.1.a workspace `members` append + "未来 crate" 注释清理

`Cargo.toml`（workspace 根）：

```diff
 members = [
     "crates/robot-adapters/feetech",
     "crates/robot-adapters/mock",
     "crates/robot-cli",
     "crates/robot-core",
     "crates/robot-health",
     "crates/robot-kinematics",
+    "crates/robot-kinematics-xlerobot",
     "crates/robot-logging",
     ...
 ]

 # 未来 crate（按 docs/01_implementation_steps.md 顺序逐步启用；...）：
-#   crates/robot-kinematics-xlerobot # v0.4+：XLeRobot 可选 IK/FK backend（若 profile 选择 IK）
    crates/robot-kinematics-piper    # v0.6+：Piper 可选 IK/FK backend（结构异构验证）
    ...
```

##### P.2.1.b `crates/robot-kinematics-xlerobot/Cargo.toml`（新）

按 Q-IK1 / Q-IK2 拍板路径落地。default 候选（沿 round 1 robot-
kinematics + robot-core 同款模板，Q-IK2 拍板**否** = 不加 nalgebra）：

```toml
[package]
name         = "robot-kinematics-xlerobot"
description  = "XLeRobot 5-DOF 手臂 IK 实现 (dimos arm_ik.py 闭式解 Rust 移植)。trait 层 clamp 行为路径 = <Q-IK16 拍板选定>（双写防漂移锚句）。"
version    .workspace = true
edition    .workspace = true
license    .workspace = true
authors    .workspace = true
repository .workspace = true
readme     .workspace = true
publish    .workspace = true

[dependencies]
robot-kinematics = { workspace = true }
# 严禁加 nalgebra / urchin / pinocchio / placo / kinpy / urdfpy 等
# IK / 矩阵 / URDF 第三方依赖 (Q-IK2 拍板 否 ACK; 如拍板 是 走
# Spec round amend + dep 显式说明)。

[lints]
workspace = true
```

##### P.2.1.b' root `Cargo.toml [workspace.dependencies]` 同步新增（**Plan round 1 amend**）

**事实**（Plan 实施期发现）：round 1 (1/1) `6caf0ee` 落 `robot-
kinematics` workspace member 时**未**同步在 `[workspace.dependencies]`
新增 path 入口（line 141-151 现有 12 `robot-*` 条目**不**含
`robot-kinematics`）。round 2 `robot-kinematics-xlerobot/Cargo.toml`
依赖 `robot-kinematics = { workspace = true }` → 需要 root workspace
deps 入口；不补会**编译 fail**。

`Cargo.toml`（workspace 根 `[workspace.dependencies]` 段；现 line 141-
151 后追加）：

```diff
 robot-core          = { path = "crates/robot-core" }
 robot-health        = { path = "crates/robot-health" }
+robot-kinematics    = { path = "crates/robot-kinematics" }
+robot-kinematics-xlerobot = { path = "crates/robot-kinematics-xlerobot" }
 robot-logging       = { path = "crates/robot-logging" }
 ...
```

字母序插入位置：`robot-health` 之后、`robot-logging` 之前（跟 §P.2.1.a
workspace `members` 顺序对齐）。

**实施期分工**：本两行 workspace deps 入口跟 §P.2.1.a workspace
`members` append 同 commit land（同属 root `Cargo.toml`）；commit
节奏跨段时（如 (C) 4 commit 路径 (1/4) impl crate + (2/4) schema），
本两行属 (1/4) impl crate 段。

**Spec round 1 amend 标注**：Spec §"影响范围 / 必改文件" 表中
"`Cargo.toml`（workspace 根）" 行原文仅提 `members` append + "未来
crate" 注释清理；现 Plan round 1 amend 补 `[workspace.dependencies]`
新增 2 条 path 入口为必改项。`robot-kinematics` 入口属补 round 1
遗漏；`robot-kinematics-xlerobot` 入口属 round 2 新增。

##### P.2.1.c `crates/robot-kinematics-xlerobot/src/lib.rs`（新）

文件起首 doc 段（reviewer 审查重点 #2 第一处双写）：

```rust
//! XLeRobot 5-DOF 手臂 IK 实现。
//!
//! # 算法源
//!
//! 直接移植自 dimos `arm_ik.py`（中文 doc 完整化版本）；dimos 端
//! 上游为 XLeRobot `SO101Robot.py` `SO101Kinematics.inverse_kinematics`。
//! 5-DOF 闭式解：2-DOF 平面 IK（余弦定理）+ wrist_roll 平凡映射。
//!
//! # clamp / out-of-reach 行为路径
//!
//! **<按 Q-IK16 拍板路径填入>**：
//! - 路径 A（如选 a）：out-of-reach 时返 `Ok(IkSolution { joints_rad:
//!   clamped_to_workspace })` + 外部 informational metric
//! - 路径 B（如选 b）：返 `Err(IkError::OutOfReach { distance_m })`
//!
//! 路径选定后**3 处双写防漂移**（Spec round 1 AC-3）：
//! (1) 本 lib doc 段; (2) `Cargo.toml [package].description`;
//! (3) Spec round 1 §AC-3。
//!
//! # yaw / invalid input
//!
//! 5-DOF 手臂不可达任意 yaw 约束；yaw 处理走 Q-IK6 拍板（拒 vs ignore）；
//! 跟 clamp 路径**边界不重叠**（详 Spec AC-3 + AC-8）。
```

**类型 + impl**（具体形态按 Q-IK16 / Q-IK6 / Q-IK10 拍板路径落地）：

```rust
pub struct XLeRobotArmIkSolver {
    info: KinematicInfo,
    // 连杆参数 / 关节限位 / 偏移角（const 硬编码，沿 arm_ik.py:23-71）
}

impl XLeRobotArmIkSolver {
    pub fn new() -> Self { ... }

    // 内部辅助：闭式 2-DOF 平面 IK（沿 arm_ik.py:95-127 _planar_ik）
    fn planar_ik(distance: f64, height: f64) -> Result<(f64, f64), IkError> { ... }
}

impl ArmIkSolver for XLeRobotArmIkSolver {
    fn info(&self) -> &KinematicInfo { &self.info }

    fn solve(
        &self,
        target: &Pose,
        seed: Option<&[f64]>,
        deadline_ns: Option<u64>,
    ) -> Result<IkSolution, IkError> {
        // 1. quat → roll/pitch/yaw 欧拉角分解
        // 2. yaw 处理（Q-IK6）：拒 Err(InvalidInput { reason: "yaw not
        //    supported" }) 或 ignore
        // 3. shoulder_pan = atan2(y, x)
        // 4. 投影到臂平面 → distance / height → planar_ik → shoulder_lift
        //    + elbow_flex
        // 5. wrist_flex = pitch - 前两关节累计 pitch
        // 6. wrist_roll = roll 直接赋
        // 7. out-of-reach 处理（Q-IK16）：Ok(clamped) 或 Err(OutOfReach)
        // 8. 关节限位 clamp（沿 arm_ik.py:143-144 + :242 + :245）
        // 9. seed: ignore (闭式解不用)
        // 10. deadline_ns 处理 (按 Plan §P.3.2 #12 拍板路径 (a)
        //     backend guard + solver 不查)：
        //     - **solver 收到 deadline_ns = None**（IkBackend::start()
        //       调用方传 None，详 §P.2.3.c step 5；solver 不感知 deadline，
        //       不需要查、不需要返 IkError::DeadlineExceeded）
        //     - 闭式 5-DOF O(1) sub-µs 量级；impl crate 零 robot-core
        //       dep 不变（Q-IK2 (a) 零依赖原则保）
        //     - **trait doc 已知不适配**：round 1 lib.rs:151 写"
        //       Some(ns) 由 solver 内部自查 + 返 IkError::DeadlineExceeded"
        //       是 round 1 当时假设 solver 内部能拿到 clock 的语义；
        //       本 round 2 (a) 路径下 solver **永远收 None**，trait
        //       doc 描述的 Some(ns) 路径在本 impl 下**不会被走到**，
        //       属已知 trait doc 不适配；**不是** trait doc 含义被
        //       Plan 改写
        //     - **follow-up**：trait doc 是否需要 amend round 1（明
        //       确"caller-or-solver enforced advisory" 语义 / 或保
        //       留现有 doc + 在 round 1 spec 加 "caller 选择走外部
        //       guard 时可传 None" 兜底锚句）走**独立 reviewer round
        //       amend round 1**；本 round 2 **不**改 round 1 lib.rs /
        //       Spec 任一行（沿 Spec §"严禁改动" #1 hard boundary）
        //     - 实际 deadline enforcement 由 IkBackend::start() 用
        //       ctx.clock.now().0 pre/post solver 调用 guard，详
        //       §P.2.3.c step 4 + 6
    }
}
```

##### P.2.1.d lib unit（具体测数待 Plan 拍板）

最少 5 个 `#[test]`（沿 CodeMap 倾向 + Spec AC-3）：

```rust
#[cfg(test)]
mod tests {
    // 1. FK round-trip（取一组合法 joints → FK → solve(target) → assert
    //    解出关节弧度接近原 joints；abs <= 1e-6）
    fn fk_round_trip_basic() { ... }

    // 2. 工作空间内典型点（夹爪正前方 0.2m）→ Ok(IkSolution)
    //    + 关节限位内
    fn solve_workspace_interior_returns_ok() { ... }

    // 3. 工作空间边界外（距离过远）→ assert 选定路径生效
    //    （如 Q-IK16 (a) → Ok(clamped within workspace boundary)；
    //    如 Q-IK16 (b) → Err(IkError::OutOfReach { distance_m })）
    fn solve_out_of_reach_follows_q_ik16_path() { ... }

    // 4. yaw != 0 quat → assert 选定路径生效（Q-IK6 (a) → Err(
    //    IkError::InvalidInput { reason }); (b) → Ok + 忽略 yaw）
    fn solve_nonzero_yaw_follows_q_ik6_path() { ... }

    // 5. 关节限位 clamp（target 需要 joint 超 max_rad → assert 输出
    //    joints_rad 全部在 [min_rad, max_rad] 内）
    fn solve_respects_joint_limits() { ... }

    // 可选 6. NaN translation → Err(IkError::InvalidInput)
    fn solve_nan_translation_returns_invalid_input() { ... }
}
```

##### P.2.1.e Quality Gate（impl crate 段）

- `cargo fmt --all --check` clean
- `cargo clippy --workspace --all-targets -- -D warnings` 双路 clean
  （含 / 不含 `robot-rpc/client` feature）
- `cargo build --workspace --release` 过
- `cargo build -p robot-kinematics-xlerobot --release` 过
- `cargo test -p robot-kinematics-xlerobot --lib` 全过
- `cargo tree -p robot-kinematics-xlerobot --edges normal,no-proc-macro`
  直接 dep 含 `robot-kinematics`；**不**含 `nalgebra` 等（如 Q-IK2 拍
  板 否；如 拍 是 走 Spec amend）
- `cargo doc -p robot-kinematics-xlerobot --no-deps` clean（AC-3 双
  写锚句 render 通过）
- `cargo test -p robot-kinematics --lib`：round 1 既有 4 lib unit
  **0** 回归（trait crate **不**改）
- `cargo test --workspace --tests --features "robot-rpc/client" --
  --test-threads=1` workspace 既有 963+ PASS **0** 回归

#### P.2.2 [schema 注册链 段] `IkPayload` + 三层 `ActionKind::Ik` atomic

##### P.2.2.a `crates/robot-msg` schema 注册链 6 步（沿候选 1 同款）

| # | 文件 | 当前 | 改动 |
|---|---|---|---|
| 1 | `crates/robot-msg/schemas/ik_payload.fbs`（新）| — | `namespace robot.msg;` + `table IkPayload { <按 Q-IK4 拍板字段集> }` + `root_type IkPayload;` |
| 2 | `crates/robot-msg/build.rs:57` | `current_schemas: [&str; 13]` | → `[&str; 14]` + append `"ik_payload.fbs"` |
| 3 | `crates/robot-msg/src/schema_id.rs:106` | `SCHEMA_SCRIPTED_PAYLOAD = 0x0000_040B`（当前最大）| append `pub const SCHEMA_IK_PAYLOAD: u32 = 0x0000_040C;` + `SCHEMA_REGISTRY:128-159` 升序末尾 entry `(SCHEMA_IK_PAYLOAD, "robot.msg.IkPayload")`（21 → 22 条）|
| 4 | `crates/robot-msg/src/markers.rs:75-100` | `ScriptedPayload` impl 模板 | 新 `pub struct IkPayload;` + `impl BodyMarker for IkPayload { SCHEMA_ID = schema_id::SCHEMA_IK_PAYLOAD; SCHEMA_NAME = "robot.msg.IkPayload"; BLOB_EMBEDDED = false; type Root<'a> = msg::IkPayload<'a>; }`（沿 `JointState` / `ScriptedPayload` 同款风格）|
| 5 | `crates/robot-msg/src/size_tier.rs:62-97` `default_tier_of` match arm | `Control` 段含 `SCHEMA_SCRIPTED_PAYLOAD`（line 95）| 同 arm 加 `SCHEMA_IK_PAYLOAD => SizeTier::<按 Q-IK5 拍板>`（CodeMap 倾向 `Control`；本 Plan **不**预决）+ `tests::default_tier_of` 穷举自动覆盖（依赖 `SCHEMA_REGISTRY` 遍历）|
| 6 | `crates/robot-msg/src/build_helpers.rs`（**可选**）| 候选 1 拍板 (a) inline 复制 → 跳过 | `build_ik_payload(...)` helper —— reviewer Plan round 拍板（沿候选 1 同款"inline 复制"倾向；如 reviewer 拍板加 helper 走 Plan-Execute diff）|

**`ik_payload.fbs` 字段集**（按 Q-IK4 + Q-IK7 拍板路径落地；Plan 初
版**不**预决）：

最小集候选（CodeMap §3.4.c）：
```fbs
namespace robot.msg;

table IkPayload {
  target_xyz: [double];              // 长度 == 3
  target_quat_xyzw: [double];        // 长度 == 4
  seed_joints_rad: [double];         // 空 = 无 seed
  frame: string;                     // 空 = solver default（Q-IK7
                                     // 拍板纳入 vs 不纳入）
}

root_type IkPayload;
```

扩展集（待 Q-IK4 拍板**是否纳入** round 2）：`tolerance_pos_m` /
`tolerance_rot_rad` / `arm_id` / `priority` / `on_timeout` /
`max_segment_duration_ms` 等 —— Plan **不**预决。

##### P.2.2.b 三层 `ActionKindProto::ActionKindIk` enum append + 下游映射 atomic

> 沿候选 1 reviewer Plan round 1 阻断 1 锁定模式：enum append + 下游
> dispatch 映射 **必须**同 commit 内 atomic，否则编译 fail。

| # | 层 / 文件 | 改动 |
|---|---|---|
| 1 | `crates/robot-rpc/proto/robot_control.proto:119-124` | `ActionKindProto` enum append `ACTION_KIND_IK = 4`（prost 生成名 `ActionKindProto::ActionKindIk`，沿 `ActionKindScripted` CamelCase + enum prefix 同款）|
| 2 | `crates/robot-msg/schemas/action_request.fbs:38-43` | `ActionKindFb` enum append `Ik = 4`（不动现有 variant 编号）|
| 3 | `crates/robot-s1/src/action.rs:148-162` | `ActionKind` enum append `Ik` variant（沿 `DwellMs` / `HoldPose` / `Scripted` 同款）|
| 4 | `crates/robot-rpc/src/task_handlers.rs:1387-1394` `action_kind_proto_to_fb` | match arm 加 `ActionKindProto::ActionKindIk => ActionKindFb::Ik`；exhaustive 不留 `_` 兜底 |
| 5 | `crates/robot-rpc/src/task_handlers.rs:1546` `EXPECTED_VARIANT_COUNT: usize = 3` | → `4`（沿候选 1 同款 variant count 锁测扩） |
| 6 | `crates/robot-s1/src/rr_server/handler.rs:178-181` `ActionKindFb → ActionKind` | match arm 加 `ActionKindFb::Ik => ActionKind::Ik`；exhaustive |
| 7 | `crates/robot-s1/src/rr_server/handler.rs:645+` `ActionKindFb → ActionKind` variant count 锁测 | 同步扩 3 → 4 |
| 8 | `crates/robot-s1/src/action.rs` `action_kind_three_v04_variants` 测 | 更新断言：variant 集 = `{DwellMs, HoldPose, Scripted, Ik}` 四项 + HashSet 长度 = 4；测名同步改 `action_kind_four_v04_variants`（沿候选 1 (1/3) 同款 重命名模式）|

##### P.2.2.c gateway 层 IK payload byte cap（可选；按 Q-IK5 / Plan 子项拍板）

如 Q-IK5 size_tier 或 Plan payload cap 子项拍板需要：

| 步骤 | 改动 | 锚 |
|---|---|---|
| 1 | `crates/robot-rpc/src/task_handlers.rs` 顶部加 `pub(crate) const MAX_IK_PAYLOAD_BYTES: usize = <按拍板值>;` | 沿候选 1 `MAX_SCRIPTED_PAYLOAD_BYTES = 4 * 1024` 同款 |
| 2 | `handle_invoke_task` validate 阶段加 `if kind == ActionKindProto::ActionKindIk && cmd.payload.len() > MAX_IK_PAYLOAD_BYTES { return tonic_invalid_argument(...); }` | 沿候选 1 gate 1 同款 |
| 3 | `CONTROL_TIER_MAX_BYTES = robot_msg::size_tier::SizeTier::Control.max_bytes()` 单源绑定**已有**（候选 1 land）→ gate 2 自动覆盖 IK | 沿候选 1 |
| 4 | lib unit 加 `invoke_task_ik_payload_over_max_bytes_returns_invalid_argument`（mock cmd payload = `[0u8; MAX_IK_PAYLOAD_BYTES + 1]`）| 沿候选 1 同款 |

> **本步骤可选**：如 Q-IK5 拍 `Control` 且 Plan 不需要更严的 IK 专用
> cap → 跳过本步骤，仅靠 `CONTROL_TIER_MAX_BYTES` gate 2 兜底。

##### P.2.2.d Quality Gate（schema 段）

- `cargo run -p robot-msg --bin schema_lint` 过（`SCHEMA_IK_PAYLOAD
  = 0x040C` 唯一 + 升序 + 未与 deprecated 冲突）
- `cargo test -p robot-msg` 全过（schema_lint + `default_tier_of`
  穷举 + `SCHEMA_REGISTRY` 长度自动覆盖）
- `cargo test --workspace --tests --features "robot-rpc/client" --
  --test-threads=1`：
  * `action_kind_four_v04_variants` PASS（替换 three 锁测）
  * `action_kind_proto_to_fb_variant_count_matches_explicit_mapping`
    `EXPECTED_VARIANT_COUNT = 4` PASS
  * `action_kind_fb_variant_count_matches_explicit_mapping` 同步扩
    PASS
  * 现有 963 PASS 全保 + 上述 3 测 + 可选 byte cap lib unit
- 双路 `crash_restart_smoke` 5/5 PASS（real-s1）+ informational
  （mock-s1）
- 严禁 `_` 兜底：任一 exhaustive match 漏接 variant 直接 build fail

##### P.2.2.e 风险锚

- **三层 enum append + dispatch 必须 atomic**（沿候选 1 reviewer
  Plan round 1 阻断 1 同款锁定）；任一层 append 但 dispatch / 锁测
  未同 commit land → 编译 fail
- **`SCHEMA_IK_PAYLOAD = 0x0000_040C` 占位**：如 round 2 实施期发现
  schema_id 冲突（极不可能；CodeMap §3.4.a 已验证下一空位）→ Plan-
  Execute diff 拍板新槽位
- **如 Q-IK5 拍 `Control` 路径的隐含上限**：IK 单 pose payload
  ~50-100 B << Control 8 KiB；**容量边界**归 **Q-IK5 size_tier** +
  Plan §P.3.2 #7 gateway payload cap 子项（**不**挂 Q-IK4 字段集语
  义层）；如 Q-IK4 字段集**扩展**导致 payload bytes 超 Q-IK5 拍板 tier
  上限 → 走 Plan-Execute diff 由 Q-IK5 / Plan #7 重新拍 size_tier 或
  byte cap。**注**：Q-IK5 拍板前**不**预决 size_tier 选项；本段仅描
  述 `Control` 路径下的容量数学，非 Q-IK5 已拍板事实

#### P.2.3 [S1 wiring 段] Coordinator solver 字段 + ActionContext + IkBackend

##### P.2.3.a `Coordinator` solver 字段（按 Q-IK3 拍板路径落地）

按 Spec AC-6 条件化 4 分支，**Plan 阶段 P.3 #5 拍板** 后才锁路径。
默认候选 (a)（reviewer 入口锚定）：

`crates/robot-s1/src/coordinator.rs:1310-1322` `Coordinator` struct
新字段：

```rust
pub struct Coordinator {
    inner: Mutex<InnerState>,
    enabled_backends: Vec<String>,
    registry: BackendRegistry,
    config: CoordinatorConfig,
    hardware: HardwareSection,
    layout_id: u64,
    anchor_bias: Vec<f64>,
    // 新字段（路径 a/b/c 落地，路径 d 不加）：
    solver: Option<Box<dyn ArmIkSolver>>,  // (a) 默认
    // solver: Option<Arc<dyn ArmIkSolver>>,        // (b) 备选
    // solvers: HashMap<ArmId, Box<dyn ArmIkSolver>>,  // (c) 多臂
                                                     // (违反 hard
                                                     // boundary)
}
```

`Coordinator::new()` 签名扩展（位置 / builder / setter 待 Plan §P.3
#5b 子拍板）：

```rust
pub fn new(
    config: CoordinatorConfig,
    hardware: HardwareSection,
    layout_id: u64,
    anchor_bias: Vec<f64>,
    registry: BackendRegistry,
    enabled_backends: Vec<String>,
    solver: Option<Box<dyn ArmIkSolver>>,  // 新参数末尾追加
                                          // (具体位置 P.3 拍板)
) -> Self { ... }
```

Startup fail-fast 路径（按 Q-IK10 拍板路径落地）—— Spec AC-6 条件
化已锁；Plan 阶段 P.3 #6 拍板具体 a/b/c 分支：
- (a) feature off = no register = `NoBackendForKind`（最简；倾向）
- (b) feature on + solver = None = lazy `Rejected`
- (c) `Coordinator::new()` fail-fast `Err(InitError::SolverMissing)`

**关节名一致性校验**（按 Q-IK10 续子项拍板）：
- (i) `Coordinator::new()` 期 `solver.info().joint_names ==
  hardware.joints.iter().map(|j| j.name)` → fail-fast
- (ii) `IkBackend::start()` 期延迟校验 → `ActionStartError::Rejected`

##### P.2.3.b `ActionContext::solver` 注入

`crates/robot-s1/src/action.rs:352-358` `ActionContext` struct 新字
段（按 Q-IK3 (a)/(b) 路径）：

```rust
pub struct ActionContext<'a> {
    pub clock: &'a dyn RobotClock,
    pub layout_id: u64,
    pub hardware: &'a HardwareSection,
    pub config: &'a CoordinatorConfig,
    pub anchor_bias: &'a [f64],
    pub command_seq: u64,
    pub solver: Option<&'a dyn ArmIkSolver>,  // 新（路径 a/b）
}
```

注入点：`run_action_tick_loop` / `Coordinator::tick_action()` 构造
`ActionContext` 时 borrow `self.solver.as_deref()`（具体注入点位置
待 Plan 拍板，**不**改 `run_action_tick_loop` 总体 contract）。

##### P.2.3.c `IkBackend` impl（按 Q-IK17 + Q-IK8 拍板路径落地）

新文件：`crates/robot-s1/src/action_backend/ik.rs`

```rust
pub struct IkBackend;

inventory::submit! { &IkBackend as &dyn ActionBackend }

impl ActionBackend for IkBackend {
    fn name(&self) -> &'static str { "ik" }
    fn supported_kinds(&self) -> &'static [ActionKind] {
        &[ActionKind::Ik]
    }
    fn supported_schemas(&self) -> &'static [u32] {
        &[SCHEMA_IK_PAYLOAD]
    }
    fn start(
        &self,
        req: &ActionRequest,
        ctx: &ActionContext<'_>,  // shared borrow（`action.rs:452` trait
                                  // 真实签名 = `&ActionContext`；**不**
                                  // 是 `&mut`；solver borrow / deadline
                                  // guard 都不需要 mut）
    ) -> Result<Box<dyn ActionInstance>, ActionStartError> {
        // 1. payload_schema_id 校验
        // 2. flatbuffers::root::<IkPayload>(payload) verify
        // 3. 取 solver: ctx.solver.ok_or(ActionStartError::Rejected(
        //    "no IK solver wired"))? (路径 a/b/c)
        //    或 (路径 d) IkBackend::SOLVER static 直接拿
        // 4. **deadline pre-guard (按 Plan §P.3.2 #12 (a) 拍板)**：
        //    let now_ns = ctx.clock.now().0;
        //    let deadline_ns = req.deadline_ns;  // ActionRequest.deadline_ns: u64
        //    if now_ns >= deadline_ns {
        //        return Err(ActionStartError::Rejected(format!(
        //            "solve deadline exceeded before start: now={now_ns} \
        //             deadline={deadline_ns}"
        //        )));
        //    }
        // 5. **调用 solver，deadline_ns 传 None**（Plan §P.3.2 #12
        //    (a) backend guard 路径明文要求）：
        //    solver.solve(target, seed, /* deadline_ns: */ None)
        //    → Ok(IkSolution { joints_rad }) → 继续
        //    → Err(IkError) → 按 Q-IK8 拍板映射 ActionStartError
        //      （IkError::DeadlineExceeded 在本 (a) 路径下 solver
        //      收 None **永不返**；映射表 placeholder，不实现 lib unit；
        //      详 Plan §P.3.2 #12 + §P.2.3.d 错误映射表 DeadlineExceeded
        //      行）
        // 6. **deadline post-guard (按 Plan §P.3.2 #12 (a) 拍板)**：
        //    let after_ns = ctx.clock.now().0;
        //    if after_ns >= deadline_ns {
        //        return Err(ActionStartError::Rejected(format!(
        //            "solve deadline exceeded after solve: after={after_ns} \
        //             deadline={deadline_ns}"
        //        )));
        //    }
        // 7. 按 Q-IK17 拍板 single-shot vs trajectory-like 返:
        //    (a) single-shot: Box::new(IkInstance::SingleShot {
        //        target_joints, done: false })
        //    (b) trajectory-like: Box::new(IkInstance::Trajectory {
        //        target_joints, ... }) (沿 ScriptedBackend 模板)
    }
}
```

**deadline enforcement 单层架构**（Plan §P.3.2 #12 (a) 拍板路径；
reviewer Plan round 1 amend 锁定）：

| 层 | 位置 | 行为 |
|---|---|---|
| **Backend layer guard** | `IkBackend::start()` step 4 + 6 | 用 `ctx.clock.now().0` pre/post solver 调用做 guard；超 deadline → `ActionStartError::Rejected("solve deadline exceeded ...")` → `FailedPrecondition` |
| **Solver 不感知 deadline** | `IkBackend::start()` step 5 调用 `solver.solve(target, seed, None)` | `XLeRobotArmIkSolver::solve()` 收 `deadline_ns: None`；solver 不感知 deadline / 不查 / 永不返 `IkError::DeadlineExceeded` |

**为何 solver 传 `None` 不传 `Some(deadline_ns)`**（reviewer Plan
round 1 amend 锁定路径；避免暗改 round 1 trait doc）：
- round 1 trait doc `lib.rs:151` 写 "`Some(ns)` 由 solver 内部自查 +
  返 `IkError::DeadlineExceeded`" —— 是当时假设 solver 能拿合法 clock
  的语义；本 round 2 (a) backend guard 路径下 solver 无 clock 源
  （impl crate 零 `robot-core` dep）
- 传 `Some(deadline_ns)` 但 solver 永不查 = **暗改 trait doc 语义**
  （改成 "solver 可以不查的 hint"）；reviewer 阻断
- 传 `None` = 显式告诉 solver "本调用不需要 deadline enforcement"，
  对 trait doc 语义**零破坏**（trait doc 描述 `None` = 无 deadline，
  正是 backend guard 路径下的语义）
- 真实 deadline enforcement 由 backend layer step 4 + 6 完成；caller
  收到 `Rejected("solve deadline exceeded ...")` → `FailedPrecondition`
  覆盖 trait doc 期望的 enforcement 语义

**已知不适配 + follow-up 登记**（reviewer Plan round 1 amend 显式登
记）：
- round 1 trait doc 描述 `Some(ns)` 路径在本 impl 下**永不被走到**
  （`IkBackend` 永远传 `None`）；属**已知不适配**，**不是** trait
  doc 含义被 Plan 改写
- 如未来其它 impl crate（如 `robot-kinematics-piper` v0.6+）需要
  `Some(ns)` 真正路径 → 走**独立 reviewer round amend round 1**
  添 advisory / caller-or-solver enforced 语义；本 round 2 **0** 触
  round 1 trait crate（详 Spec §"严禁改动" #1）
- impl crate `lib.rs` 顶模块 doc + Cargo.toml `[package].description`
  双写登记 "`XLeRobotArmIkSolver::solve()` 实现仅响应 `deadline_ns
  = None` 路径；caller 调用方需走 backend layer guard 做 deadline
  enforcement"

`IkInstance::tick` 输出（按 Q-IK17 拍板）：
- 路径 (a) single-shot：
  ```rust
  // 第一次 tick: build_hold_command(ctx.config, ctx.layout_id,
  //   &self.target_joints, ctx.command_seq, ctx.clock.now().0)
  //   → Continue { progress: 1.0, command: Some(body) }
  // 第二次 tick: Completed(ActionOutcome::Success)
  ```
- 路径 (b) trajectory-like：
  ```rust
  // 沿 ScriptedBackend tick 模板:
  // - phase 状态机 (Interp -> Dwell)
  // - 插值 from = anchor_bias (initial) to = target_joints
  // - build_hold_command(..., ctx.command_seq, ...) 每 tick
  // - 末段 Dwell 完 -> Completed(Success)
  ```

**`source_task_id == ctx.command_seq` 严格单调契约**（B3 §3.7 +
Spec AC-7 + reviewer 审查重点 #1）：
- `IkInstance::tick` 输出 `JointCommand` **必须**调
  `build_hold_command(cfg, layout_id, bias, ctx.command_seq, now_ns)`
  （`coordinator.rs:362-378`）；**严禁** backend 私有 counter / wall
  clock / 任何绕过 `ctx.command_seq` 路径
- lib unit `ik_instance_tick_source_task_id_eq_ctx_command_seq`
  bit-equivalent 锁测（沿候选 1 `tick_segment_returns_command_with_
  source_task_id_eq_ctx_command_seq` 同款）

##### P.2.3.d 错误映射（按 Q-IK8 拍板路径落地）

**两层错误分类**（reviewer Plan round 1 amend 锁定，沿候选 1
`ScriptedBackend` + B4 `wire.rs::submit_err_to_rpc_status:340`
实测口径）：

**Layer 1 — wire-level decode fail**（`IkBackend::start()` 接到
payload 后的 bytes 解析阶段；**不**走 `IkError`）：
- `payload_schema_id != SCHEMA_IK_PAYLOAD` → `ActionStartError::
  Rejected(format!("expected IkPayload schema_id, got {got:#x}"))`
- `flatbuffers::root::<IkPayload>(payload)` verify fail → `ActionStartError::
  PayloadDecode(format!("IkPayload root verify failed: {err}"))`
- 字段读 fail（如 `target_xyz` 不是 `Vec` / `frame` 非 UTF-8）→
  `ActionStartError::PayloadDecode(format!("..."))`

两者通过 `submit_err_to_rpc_status` (`wire.rs:340`) 都映射到
**`ActionRpcStatus::FailedPrecondition`**（候选 1 land 实测口径；
**不**走 `InvalidArgument` —— `InvalidArgument` 留给 gateway 层
validate 阶段，如 `kind == Unspecified` / payload byte cap 超）。

**Layer 2 — solver semantic invalid**（`solver.solve(target, seed,
deadline_ns)` 返 `Err(IkError)`；走映射表）：

`IkError` 5 variant → `ActionStartError` / `ActionFailure` 映射候选
（Plan §P.3 #8 拍板；**注意：`IkError::InvalidInput` 走 `Rejected`
**不**走 `PayloadDecode` —— 后者保留给 wire-level FBS decode 失败，
两类语义解耦**）：

| `IkError` | 候选 A (start-only) | 候选 B (start + tick) |
|---|---|---|
| `InvalidInput { reason }`（yaw unsupported / seed.len() != joint_count / quat 非规范化 等 solver semantic reject）| `ActionStartError::Rejected(format!("invalid IK input: {reason}"))` → `FailedPrecondition`（**不**走 `PayloadDecode`） | 同 |
| `NoSolution` | `ActionStartError::Rejected("no IK solution")` → `FailedPrecondition` | 同 |
| `OutOfReach { distance_m }` | **如 Q-IK16 (a) clamp 路径** → solver 返 `Ok`，**不**冒泡到 backend；**如 Q-IK16 (b)** → `ActionStartError::Rejected(format!("out of reach: {distance_m:.4} m"))` → `FailedPrecondition` | 同 |
| `SingularityTooClose { condition_number }` | `ActionStartError::Rejected(format!("near singular: cond={cond:.3e}"))` → `FailedPrecondition` | `ActionFailure::SafetyAbort` → `Internal` |
| `DeadlineExceeded` | **Plan §P.3.2 #12 (a) 拍板路径下 = unreachable**：`IkBackend::start()` step 5 调 `solver.solve(target, seed, None)` —— solver 收 `None` 永不返 `IkError::DeadlineExceeded`；实际 `Rejected("solve deadline exceeded ...")` → `FailedPrecondition` 由 `IkBackend::start()` step 4/6 backend guard 直接生成，**不**经过 `IkError` 路径；exhaustive match 仍**显式列出**本 variant（`IkError::DeadlineExceeded => unreachable!("solver never returns DeadlineExceeded under (a) backend-guard path; solve called with deadline_ns: None")`）；**不**用 `_` catch-all 兜底（保 exhaustive 锁，未来扩 `IkError` variant 编译期 fail —— 沿候选 1 exhaustive match 锁同款）；lib unit **不**强制覆盖此 variant 实际触发路径（path unreachable）| 同左（(a) 路径不区分 start/tick）|

**两层语义解耦锚句**（reviewer Plan round 1 amend 第三处双写防漂移）：

| `ActionStartError` variant | 触发层 | 触发条件 | gateway 映射 |
|---|---|---|---|
| `PayloadDecode(String)` | **wire-level** | FBS root verify fail / 字段读 fail | `FailedPrecondition` |
| `Rejected(String)` | **solver semantic** | `IkError::InvalidInput` / `NoSolution` / `OutOfReach` (Q-IK16 (b)) / `SingularityTooClose` / `DeadlineExceeded` (条件化) | `FailedPrecondition` |
| `HardwareMismatch(String)` | startup / hardware adapter（IK 路径暂不触）| — | `FailedPrecondition` |

**硬约束**（Spec §"严禁改动" + AC-8）：
- **0** 扩 `ActionFailureKindFb` 5 variant + Unknown（frozen schema）
- 5 variant 全有归属；不留 `_ => unreachable!()` 兜底
- lib unit 覆盖 5 variant 全映射（每 variant 至少 1 个 `#[test]`；
  含 wire-level decode fail 单独 lib unit `ik_backend_start_payload_
  schema_id_mismatch_returns_rejected` + `ik_backend_start_fbs_decode_
  fail_returns_payload_decode`）
- **不**复用 `PayloadDecode` 表达 solver semantic reject（reviewer 审
  查重点 #2 双层语义锁；沿候选 1 ScriptedBackend `start_rejects_empty_
  waypoints` / `start_rejects_payload_schema_id_mismatch` 同款拆分模
  式）

##### P.2.3.e `crates/robot-s1/Cargo.toml` feature flag + optional deps（按 Q-IK11 拍板）

完整改动**两段**：(1) `[dependencies]` 段新增 optional deps；(2)
`[features]` 段 enable optional deps。漏 (1) → `dep:robot-kinematics`
feature 写法**编译 fail**（Cargo 要求 dep 必须在 `[dependencies]`
显式列出 + `optional = true`）。

**(1) `[dependencies]` 段新增 optional deps**（`crates/robot-s1/
Cargo.toml:58` 现有 `[dependencies]` 段末尾追加）：

```toml
# v0.4 候选 2 round 2：IK backend 可选 dep（feature backend-ik 启用
# 时 link）。trait crate `robot-kinematics` 走 round 1 first release；
# impl crate `robot-kinematics-xlerobot` 走 round 2 first release。
# 跟 round 1 ArmIkSolver: Send + Sync 约束兼容（Box<dyn ArmIkSolver>
# 自动 Send + borrow &'a dyn 也 Sync）。
robot-kinematics          = { workspace = true, optional = true }
robot-kinematics-xlerobot = { workspace = true, optional = true }
```

**(2) `[features]` 段加 `backend-ik`**（`crates/robot-s1/Cargo.toml:
19-34` 现有 `[features]` 段；按 Q-IK11 拍板形态）：

候选 (a) Q-IK11 拍 `backend-ik` 单一（default-on；倾向）：

```toml
[features]
default = ["backend-mock-dwell", "backend-hold-pose",
           "backend-scripted",
           # 新增（按 Q-IK11 (a) 拍板）：
           "backend-ik"]
backend-mock-dwell = []
backend-hold-pose  = []
backend-scripted   = []
# v0.4 候选 2 round 2 (3/N or 等价段)：IkBackend feature gate。
# Feature off 时整 ik.rs 模块不 link → BackendRegistry::from_inventory
# 收集不到 ik backend → `[s1.profile.backends = ["ik"]]` 启动期
# fail-fast，符合 "配置与编译 profile 不匹配启动期 fail-fast"硬约束。
backend-ik = [
    "dep:robot-kinematics",
    "dep:robot-kinematics-xlerobot",
]
```

候选 (b)/(c) Q-IK11 细粒度（解耦 backend 启用 vs solver 选型；reviewer
Plan round 拍板时锁）：

```toml
# backend-ik = ["dep:robot-kinematics"]
# kinematics-xlerobot = ["backend-ik", "dep:robot-kinematics-xlerobot"]
# default = [..., "backend-ik", "kinematics-xlerobot"]
```

**(3) `crates/robot-s1/src/action_backend/mod.rs:46-53`** feature gate
mod 声明（沿 hold_pose / scripted 同款）：

```rust
#[cfg(feature = "backend-ik")]
pub mod ik;
```

**(4) root workspace 依赖入口**：详 §P.2.1.b'（同 commit land；本
段 (3/N) 实施期 `robot-s1` features 启用前提 = (1/N) impl crate +
root workspace deps 入口已 land）。

##### P.2.3.f Quality Gate（S1 wiring 段）

- `cargo fmt --all --check` clean
- `cargo clippy --workspace --all-targets -- -D warnings` 双路 clean
  + 含 / 不含 `backend-ik` feature clean
- `cargo build --workspace --release` 双路过
- `cargo test -p robot-s1 --lib`：
  * `ik_instance_tick_source_task_id_eq_ctx_command_seq` bit-
    equivalent PASS
  * `IkError` 5 variant 映射全过
  * dispatch 单测 `BackendRegistry::dispatch` 返 `&IkBackend`：
    `name == "ik"` + `supported_kinds == [Ik]` + `supported_schemas
    == [SCHEMA_IK_PAYLOAD]` PASS
  * 现有 lib unit **0** 回归
- `cargo test --workspace --tests --features "robot-rpc/client" --
  --test-threads=1` 全过

#### P.2.4 [e2e smoke 段] `ik_backend_smoke` binary + cross-process

##### P.2.4.a `crates/robot-tests/Cargo.toml`

```toml
[[bin]]
name = "ik_backend_smoke"
path = "src/bin/ik_backend_smoke.rs"
required-features = ["ik-backend-smoke"]

[features]
ik-backend-smoke = [
    "dep:robot-rpc",
    "dep:tonic",
    "dep:tokio",
    "robot-s1/backend-ik",  # 强依赖（feature 名按 Q-IK11 拍板）
]
```

##### P.2.4.b CLI args（沿 `scripted_backend_smoke` 同款）

- `--bin-dir target/release`（默认）
- `--robot-id-prefix ik_backend`（默认）
- `--hardware-id <id>`（required）/ `--config <toml>`（required）
- `--phase <按 Q-IK13 拍板 phase 集合>`（默认 `all`）
- `--output-csv <path>`（可选）

##### P.2.4.c `ik_backend_smoke.rs` 复制 helper（按 Q-IK12 拍板）

沿候选 1 / 3 同款 inline 复制模板（Q5(a) 候选 1 拍板路径 A；如 Q-IK12
拍板 (b) 抽公共 crate → 走 Spec round amend）：

| helper | 来源 |
|---|---|
| `FakeStatePublisher` | `scripted_backend_smoke.rs` inline 等价 |
| `SubprocessHandle` + `bring_up_pair` + `tear_down_pair` | `scripted_backend_smoke.rs` inline 等价 |
| `invoke_and_wait_terminal` | `scripted_backend_smoke.rs` inline 等价 |
| `build_ik_payload(target_xyz, target_quat_xyzw, seed, frame) -> Vec<u8>` | **新增** inline fbs builder（沿候选 1 同款 Q5(a)）|

##### P.2.4.d AC 实施序列（按 Q-IK13 拍板 phase 集合）

CodeMap 倾向 5 phase（具体待 Q-IK13 拍板）：

| Phase | 验证 | Q 引用 |
|---|---|---|
| `selftest` | smoke binary 自检（gateway 通 + fake S0 publish + `IkPayload` fbs build → root verify）| - |
| `happy_single_target` | 工作空间内 pose → `InvokeTask` + Stream Started + ≥1 Progress + `Completed(Success)` + `JointCommand.positions[]` 跟 solver 解出 joints 一致 | AC-10 happy |
| `source_task_id_monotonic` | 多 IK action 连续 → `JointCommand.source_task_id` 严格单调 | AC-7 + B3 §3.7 |
| `unreachable` | 出工作空间 pose → 按 Q-IK16 拍板路径验证（(a) → `Completed(Success)` + clamped target；(b) → `Failed(PreconditionFailed)`）| AC-3 + AC-10 |
| `invalid_input` | NaN translation / 非规范化 quat / yaw != 0（按 Q-IK6） / seed.len() != joint_count → `Failed(PreconditionFailed)` 或 gateway `InvalidArgument` | AC-8 + AC-10 |

##### P.2.4.e `configs/ik_backend_smoke.toml`（新）

```toml
robot_id = "ik_backend_smoke"

[hardware]
hardware_id = "smoke_arm"
joints = [
    # XLeRobot 5 arm joints（具体顺序按关节名一致性校验 P.2.3.a
    # Q-IK10 续子项拍板）
]

[s1.profile.backends]
enabled = ["ik", "hold_pose"]

# 可选: [kinematics] xlerobot solver 选型字段（按 Q-IK11 拍板）
```

##### P.2.4.f Quality Gate（e2e 段）

- `cargo build --release -p robot-tests --bin ik_backend_smoke
  --features ik-backend-smoke` 过；生产构建（无 feature）`cargo
  build -p robot-tests --release` **不**含本 binary
- `cargo test -p robot-tests --bin ik_backend_smoke --features
  ik-backend-smoke` 过（沿候选 2 round 1 §P.3.3a (v) per-bin
  cargo test 同款 —— **不**用 `--tests` 整体）
- `./target/release/ik_backend_smoke --phase all --hardware-id
  smoke_arm --config configs/ik_backend_smoke.toml` exit code 0；
  按 Q-IK13 拍板的 phase 集合全部 PASS
- 双路 `crash_restart_smoke` 5/5 PASS（real-s1）+ informational
  （mock-s1）
- **重型 smoke 升回 (a) 硬 gate**（沿 Spec AC-Q 已锁；Plan 阶段不重
  新拍板）：
  * `pubsub_cleanup_smoke --phase all`：exit 0 + AC-A1/A2/A3 PASS
    + AC-C1/C2/C3 trigger 全 `false` + `events_registry_len final
    == 0`
  * `scripted_backend_smoke --phase all`：exit 0 + 5 phase 全
    PASS（对齐候选 1 summary §4.1）
  * `ik_backend_smoke --phase all`：exit 0 + 按 Q-IK13 拍板 phase
    集合全 PASS

#### P.2.5 [summary 段] 归档

新文件：`docs/handoffs/v0.4_robot_kinematics_round2_summary.md`（沿
候选 1 / round 1 同款模板）。

| 段 | 内容 |
|---|---|
| §1 范围 | round 2 = robot-kinematics-xlerobot impl + IkBackend + S1 wiring + e2e smoke |
| §2 commit chain | CodeMap → Spec → Plan → 实施 (按拍板节奏) → summary 完整 hash |
| §3 关键契约 | (a) round 1 trait crate 0 改动；(b) B3 §3.7 source_task_id == ctx.command_seq；(c) 不预钉 XLeRobot clamp 行为（round 1 AC-6 沿用 + round 2 XLeRobotArmIkSolver 选定路径 3 处双写）；(d) IkError 5 variant 错误映射全有归属 + 0 扩 frozen ActionFailureKindFb；(e) `SCHEMA_IK_PAYLOAD = 0x040C` 注册链 atomic；(f) §6.7 5 不变量真宣称保持 |
| §4 AC + Quality Gate 实测 | AC-1..AC-11 + AC-Q 全过记录 + 重型 smoke 3 binary 全过 |
| §5 §6.7 5 不变量 | **0** 降级 / **0** 升级 / **0** 偏移 |
| §6 后续 | 候选 2 整体完结锚点 + 未来 reviewer round 列表（`BaseKinematics` / 多臂 dispatch / URDF + frame transform / `robot-kinematics-piper`）|
| §7 Plan-Execute diff | 实施期偏离 Plan 归档 |

同步更新：`docs/handoffs/README.md` anchor + `docs/CHANGELOG.md` +
`CLAUDE.md` 状态段。

### P.3 reviewer Plan 拍板项

> 下列项**未**在 Spec round 1 ACK；reviewer Plan round 1 拍板后锁
> Plan v2 / Plan-Approved，进入实施。**Spec Q-IK1..Q-IK13 + Q-IK16
> ..Q-IK17 跟 Plan 拍板项合并拍** —— reviewer 一次性 ACK 两组 Q +
> 项后才可进 (1/N) 实施。

#### §P.3.1 Q-IK1..Q-IK13 + Q-IK16..Q-IK17 集中拍板

> Spec round 1 已**列出 15 Q + 候选**；Plan 阶段**集中拍板锁定路径**。
> CodeMap Q-IK14（`cargo deny` baseline）/ Q-IK15（§6.7 触碰）**已下
> 沉到 hard boundary**（详 §P.0），**不**重新拍。

| # | Q | reviewer 拍板选项 | plan author 倾向（仅参考；待拍板）|
|---|---|---|---|
| Q-IK1 | ~~impl crate workspace dep 入口~~ → **已 Plan round 1 amend 锁 (b) 双 workspace dep 入口**（编译可行性强制；详 §P.2.1.b'：`robot-kinematics-xlerobot/Cargo.toml` 依赖 `robot-kinematics = { workspace = true }` 必须 root `[workspace.dependencies]` 有 `robot-kinematics` 入口，否则 cargo `workspace = true` 解析 fail；`robot-s1/Cargo.toml` 用 `dep:robot-kinematics-xlerobot` feature 必须 root 有 xlerobot 入口；两个都补，**0** 商榷空间） | **不再作 reviewer 拍板 Q** —— Plan round 1 amend 锁 (b)；本 Q 槽位保留是为编号稳定 / CodeMap 引用一致性 |
| Q-IK2 | impl crate `nalgebra` 可选 feature | (a) 否 / (b) 是 optional feature | (a) |
| Q-IK3 | Solver ownership 形态 | (a) `Coordinator::solver: Option<Box<dyn>>` + `ActionContext::solver: Option<&'a dyn>` / (b) `Arc<dyn>` 替 Box / (c) HashMap 多臂（违反 hard boundary）/ (d) IkBackend 自持 static | (a) |
| Q-IK4 | `IkPayload` 字段集 | (a) 最小集（target_xyz + target_quat_xyzw + seed_joints_rad + frame）/ (b) 扩展集（+ tolerance / arm_id / 其它）| (a) |
| Q-IK5 | `SCHEMA_IK_PAYLOAD` size_tier | (a) `Control` (8 KiB) / (b) `SensorSmall` (1 MiB) / (c) 其它 | (a) |
| Q-IK6 | 5-DOF yaw 处理策略 | (a) yaw != 0 → `Err(IkError::InvalidInput { reason })` / (b) ignore | (a) |
| Q-IK7 | `IkPayload.frame: string` 字段是否纳入 round 2 | (a) 纳入（占位字符串；实际 frame 变换推未来）/ (b) 不纳入 | (a) |
| Q-IK8 | `IkError` 5 variant → 上层错误阶段 | (a) 全 `ActionStartError`（闭式解全 start 期）/ (b) 拆 start vs tick | (a) |
| Q-IK9 | `ActionFailureKindFb` 第 6 variant `IkSolverError` 扩展 | (a) 否（保 5 + Unknown frozen；用 `PreconditionFailed` 兜底）/ (b) 是（违反 hard boundary，需 reviewer 解锁）| (a) |
| Q-IK10 | Solver 缺失时 fail-fast 策略（**reviewer Plan round 1 amend 拆两层 + round 2 amend 修正测试形态**） | (a) **双层语义**：(a-i) startup fail-fast：`backend-ik` feature **off** + `[s1.profile.backends] enabled = ["ik"]` → `validate_profile_section` (`coordinator.rs:1088`) 返 `ProfileError::UnknownBackend("ik")` → `s1_daemon` 启动期 fail-fast（沿 `backend-scripted` / `backend-hold-pose` 现有同款 startup fail-fast 行为；候选 1 land 已锁）；(a-ii) runtime path：profile **未**启用 ik 但 gateway 提 `ActionKindProto::ActionKindIk` → `BackendRegistry::dispatch` 返 `SubmitError::NoBackendForKind(Ik)` → gateway `Unimplemented`（沿 candidate 1 `invoke_scripted_kind_without_backend_returns_unimplemented` 同款）/ (b) feature on + solver = None = lazy `Rejected`（Q-IK3 (a)/(b)/(c) Box/Arc/HashMap 路径下，`IkBackend::start()` 内 `ctx.solver.ok_or(...)?` → `ActionStartError::Rejected("no IK solver wired")`；仅当 `IkBackend` 注册但 `Coordinator::solver = None` 时触发；Q-IK3 (d) static 路径不适用）/ (c) `Coordinator::new()` fail-fast `Err(InitError::SolverMissing)`（破坏现有 `new()` 签名只返 Self；不倾向）| **(a) 双层语义**：(a-i) startup fail-fast 沿现有 `validate_profile_section` 路径（**0** 改 coordinator.rs validation 逻辑；只 ensure `BackendRegistry::from_inventory` 在 feature off 时不收 ik）+ (a-ii) runtime `NoBackendForKind` 沿现有 `BackendRegistry::dispatch` fallback；**测试分层（reviewer Plan round 2 amend 修正）**：(a-i) **不**用 `cargo test --no-default-features` integration（Plan #4 拍 default-on `backend-ik` → `cargo test -p robot-s1` 默认含 ik 跑不到 feature-off 真实路径；且 `--no-default-features` 路径需先确认 robot-s1 整体能编译过，引入额外验证成本）；改用 **`BackendRegistry::from_explicit(&[])`** 显式构造**不含 ik backend 的 registry**（API 已存在 `crates/robot-s1/src/action_backend/mod.rs:123`），调 `validate_profile_section(&profile, &empty_registry, ...)` with `profile.backends = vec!["ik".into()]` → assert `Err(ProfileError::UnknownBackend("ik"))`；lib unit 测名 `validate_profile_section_with_ik_backend_missing_returns_unknown_backend`（**不**声称是 cargo feature-off integration；显式测 `validate_profile_section()` 的 `UnknownBackend` 路径覆盖；feature-off 真实路径回归靠现有 `backend-scripted` / `backend-hold-pose` 同款锚句沿用）；(a-ii) lib unit 加 `dispatch_ik_kind_without_ik_backend_returns_no_backend_for_kind`（沿 candidate 1 `invoke_scripted_kind_without_backend_returns_unimplemented` 同款；用 `from_explicit(&[])` 同款构造）；**(b) Q-IK3 (a)/(b)/(c) 路径补做** lib unit `ik_backend_start_with_no_solver_wired_returns_rejected` |
| Q-IK10b | 关节名一致性校验位置（Q-IK10 续子项）| (i) `Coordinator::new()` 期 startup fail-fast / (ii) `IkBackend::start()` 期延迟校验 | (i) |
| Q-IK11 | feature flag 粒度 | (a) `backend-ik` 单一 default 进 + 默认 wired XLeRobot solver / (b) `backend-ik` + `kinematics-xlerobot` 双 feature 解耦 / (c) per-robot 多 feature | (a) |
| Q-IK12 | smoke helper 复用 | (a) inline 复制（沿候选 1）/ (b) 抽公共 crate | (a) |
| Q-IK13 | `ik_backend_smoke` phase 集合 | (a) 5 phase（selftest / happy_single_target / source_task_id_monotonic / unreachable / invalid_input）/ (b) 4 phase（合并 unreachable + invalid_input）/ (c) 其它 | (a) |
| Q-IK16 | `XLeRobotArmIkSolver` clamp 行为路径 | (a) `Ok(IkSolution { joints_rad: clamped })` + 外部 metric（沿 dimos）/ (b) `Err(IkError::OutOfReach { distance_m })` | (a) |
| Q-IK17 | `IkInstance::tick` 输出模式 | (a) single-shot / (b) trajectory-like（沿 ScriptedBackend）| (a) single-shot（闭式解 O(1) + IK 解一次推一帧即可；trajectory-like 增复杂度无明显收益；reviewer 拍板）|

#### §P.3.2 Plan 阶段额外拍板项

| # | 项 | 选项 | plan author 倾向 |
|---|---|---|---|
| #1 commit 节奏拆段 | (A) 单 commit / (B) 3 commit / (C) 4 commit / (D) 5+ commit | (C) 4 commit（impl + schema atomic + S1 wiring + e2e；沿候选 1 (1/3)/(2/3)/(3/3) + summary 同款节奏）|
| #2 重型 smoke phase 集合（确认 Q-IK13） | 沿 Q-IK13 (a) 5 phase / 其它 | 跟 Q-IK13 同步 |
| #3 `crates/robot-kinematics-xlerobot/README.md` 是否进 round 2 | (a) 落 / (b) 不落 | (b) 不落（沿候选 2 round 1 + 候选 1 同款 README 可选；lib doc + Cargo.toml description + Spec AC-3 三处双写已足够）|
| #4 `[features] default` 是否含 `backend-ik`（Q-IK11 续子项）| (a) 进 default / (b) opt-in | (a) 进 default（沿候选 1 `backend-scripted` 同款）|
| #5 Solver ownership 形态（确认 Q-IK3 + Coordinator::new() 签名扩展形态）| Q-IK3 (a/b/c/d) + new() 参数位置 (末尾追加 / builder / setter) | Q-IK3 (a) + new() 参数末尾追加 |
| #6 Startup fail-fast 策略（确认 Q-IK10 拆两层 + 关节名校验位置）| Q-IK10 (a) 双层 (a-i startup + a-ii runtime) / (b) lazy / (c) Coordinator::new fail；关节名校验位置 (i) startup 期校验 / (ii) IkBackend::start 延迟 | Q-IK10 (a) 双层（a-i startup via `validate_profile_section` `UnknownBackend` + a-ii runtime via `NoBackendForKind` 沿现有路径，**0** 改 coordinator validation 逻辑）+ 关节名校验 (i) startup 期（沿 (a-i) 同款 startup fail-fast 路径，在 `Coordinator::new()` 或 `validate_profile_section` 扩展期校验 `solver.info().joint_names == hardware.joints[].name`）|
| #7 gateway IK payload byte cap 是否落 round 2（**Q-IK5 size_tier 容量层 / Plan payload cap 子项**；**不**归 Q-IK4 字段集语义层）| (a) 落 `MAX_IK_PAYLOAD_BYTES = ?` / (b) 不落（仅靠 `CONTROL_TIER_MAX_BYTES` gate 2） | (b) 不落（Q-IK5 拍 (a) Control 8 KiB + Q-IK4 最小集典型 payload ~50-100 B 远低于 8 KiB；候选 1 双 gate 兜底）|
| #8 错误映射阶段（确认 Q-IK8）| Q-IK8 (a/b) + 映射代码集中位置 | (a) 全 `ActionStartError` + 映射集中 `IkBackend::start()` |
| #9 `IkInstance` 输出模式（确认 Q-IK17）| Q-IK17 (a/b) | (a) single-shot |
| #10 lib unit 数量（impl crate） | (a) 5 测 / (b) ≥ 6 测 / (c) 其它 | (a) 5 测 + 可选 6 NaN translation |
| #11 summary 文件命名 | (a) `v0.4_robot_kinematics_round2_summary.md` / (b) 其它 | (a) round2 后缀 |
| #12 `deadline_ns` enforcement 位置（reviewer Plan round 1 amend 新增；reviewer Plan round 2 amend 修正路径选项） | (a) **backend guard + solver 收 None**（推荐）：`IkBackend::start()` 用 `ctx.clock.now().0` 在调 `solver.solve(target, seed, None)` **前 + 后**两次 check `req.deadline_ns`；超 deadline → `ActionStartError::Rejected("solve deadline exceeded ...")` → `FailedPrecondition`；solver 收 `None` 不感知 deadline，永不返 `IkError::DeadlineExceeded`；impl crate 零 `robot-core` dep 不变 / (b) **trait 严格路径**：`XLeRobotArmIkSolver::solve(..., Some(deadline))` 必须能查 deadline + 可能返 `IkError::DeadlineExceeded`；impl crate 加合法 clock 源（如 `robot-core` dep）+ AC-8 配套 lib unit `solve_with_short_deadline_returns_deadline_exceeded`；违反 Q-IK2 (a) 零 runtime 依赖原则 / (c) **trait doc amend round 1**：把 `deadline_ns` 改 advisory（caller-or-solver enforced）；走独立 reviewer round amend round 1 spec / lib doc | **(a) backend guard + solver 收 None**（reviewer Plan round 2 amend 锁定）；**关键调用约定**：`IkBackend::start()` step 5 调用 `solver.solve(target, seed, /* deadline_ns: */ None)`（**不**是 `Some(deadline_ns)`）；solver 不感知 deadline 是 (a) 路径的**显式调用约定**，**不**是 trait doc 改写；**trait doc 兼容性**：round 1 `lib.rs:151` 描述 `Some(ns)` 路径在本 impl 下**永不被走到**（caller 永远传 `None`）= **已知不适配**（**不是** trait doc 含义被 Plan 改写）；如未来其它 impl crate（`robot-kinematics-piper` v0.6+ 等）需要 `Some(ns)` 真正路径 → 走**独立 reviewer round amend round 1** 添 advisory 语义；本 round 2 **0** 触 round 1 trait crate（详 Spec §"严禁改动" #1）；**AC-8 配套**：lib unit `ik_backend_start_with_expired_deadline_returns_rejected`（构造 `req.deadline_ns < ctx.clock.now()` → assert `Err(ActionStartError::Rejected(_))`）；**双写防漂移**：`crates/robot-kinematics-xlerobot` `lib.rs` 顶模块 doc + Cargo.toml `[package].description` 双写登记 "`solve()` 实现仅响应 `deadline_ns = None` 路径；caller 调用方走 backend layer guard 做 deadline enforcement" 实现可观察行为锚句 |

> Q-IK1..Q-IK13 + Q-IK16..Q-IK17 + Plan #1..#12 reviewer ACK 后 Plan v2 land；偏离需
> 走 §P.4 Plan-Execute diff 模板（沿候选 1 / round 1 同款 appendix-
> only 登记机制）。

### P.4 Plan-Execute diff 登记位置

沿候选 1 / round 1 §P.4 同款模板。实施期偏离 Plan 任何决策 → 登记本
节（**不**回退 Plan，appendix-only），含：

- **触发条件**（哪个 Q-IK / Plan 拍板项被偏离）
- **实际选择**（实施期做了什么）
- **理由**（reviewer ACK / 实施期发现 / 编译期发现）
- **验证回归**（双 `crash_restart_smoke` 5/5 + AC-Q 全过 + 重型
  smoke (a) full e2e 3 binary 全过 + §6.7 5 不变量真宣称保持）

**预期可能 diff**：

- **diff #1**：`cargo deny check` overall FAIL 沿候选 2 round 1
  §P.4 diff #1 同款 baseline 不退化（**未**新增 violation；
  follow-up 独立 commit / PR）
- **diff #2**：lib unit 数量实际 vs 预估（Plan §P.3.2 #10 倾向
  5 测；如实施期加 6 NaN translation 等扩展，diff 登记）
- **diff #3**：commit 节奏拆段决定如实施期变化（如 (3/4) 内
  发现拆分诉求拆为 (3/4-a) + (3/4-b)；沿候选 3 同款）
- **diff #4**：dimos `arm_ik.py` Rust 移植期发现跟上游 `SO101Robot
  .py` 行为偏离 → 拍板对齐方向（CodeMap §6 + Spec AC-2 已锚句）
- **diff #5**：`SCHEMA_IK_PAYLOAD = 0x0000_040C` 占位冲突（极不可
  能；CodeMap §3.4.a 已验证下一空位）→ 改占位
- **diff #6**：`Cargo.lock` 范围跟 AC-11 预期偏离 —— **按 Q-IK2
  拍板条件化**：
  * 如 Q-IK2 拍 (a) 否 nalgebra → 预期 lock 仅新增 `[[package]]
    name = "robot-kinematics-xlerobot"` stanza + 依赖 `robot-kinematics`
    内行（**0** 第三方间接 dep 增量）
  * 如 Q-IK2 拍 (b) 是 optional nalgebra feature → 预期 lock 多新增
    `nalgebra` + 其传递依赖（`matrixmultiply` / `num-traits` /
    `simba` 等）多 stanza；同时需配套 `deny.toml` license + advisory
    审计（独立 follow-up，**不**捆绑 round 2）
  * 实际 lock diff 跟两路径任一预期**偏离** → 本 diff 登记
- **diff #7**：Q-IK 拍板路径实施期撞 round 1 trait 边界 → 走独立
  reviewer round amend round 1（**不**在 round 2 内暗改 trait）
- **diff #8**：mock-s1 `crash_restart_smoke` baseline regression 沿
  候选 1 mock-s1 follow-up 同款（**不**升级为新硬 gate）
- **diff #9**：测试函数命名 / 文档措辞被 workspace lint 拒（沿候
  选 2 round 1 §P.4 diff #4 同款 underscore 前缀规避；如实施期
  撞类似 lint 走同款规避）

---

## 后续：reviewer Plan round + 实施

Plan outline draft v1 commit 后，reviewer 进 **Plan round 1** 复核：
- §P.0 工作基线 / §P.1 候选拆段 / §P.2 原子步骤（文件 / 函数 / 测
  试名级别）/ §P.3 Q-IK1..Q-IK13 + Q-IK16..Q-IK17 + Plan #1..#12 集中拍板 / §P.4
  diff 登记位置
- 如有阻断 → Plan round 1+ amend commit；零阻断 → ACK 进入实施

**实施 commit 节奏**（拍板项 §P.3.2 #1 ACK 后锁；下面按倾向 (C) 4
commit 展开；如拍 (B) 3 commit / (D) 5+ commit 走 §P.1 对应路径）：

```
✨ feat: v0.4 候选 2 round 2 (1/4) robot-kinematics-xlerobot impl
        crate first release land
✨ feat: v0.4 候选 2 round 2 (2/4) IK schema 注册链 6 步 + 三层
        ActionKind::Ik enum append + dispatch 映射 atomic land
✨ feat: v0.4 候选 2 round 2 (3/4) S1 Coordinator solver 字段 +
        ActionContext::solver 注入 + IkBackend + 错误映射 land
✨ feat: v0.4 候选 2 round 2 (4/4) ik_backend_smoke binary +
        cross-process e2e
📝 docs: v0.4 候选 2 round 2 summary 归档
```

候选 2 整体完结后（**round 1 + round 2 全段**）触发下一候选（v0.4_
entry §3.1 优先级 3 → 1 → 2 → 4 完成）：候选 4 (dimos 切换) 入口
评估或 v0.5 起点拍板。
