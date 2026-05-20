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
