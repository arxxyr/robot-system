# CodeMap · v0.4 候选 2 **round 2** — `robot-kinematics-xlerobot` impl + `IkBackend` + S1 wiring + e2e

> **预研性 Pre-Research**（沿候选 1 / 3 + 候选 2 round 1 同款）—— 仅
> **定位** round 2 实施期所有触点 + **列举** 待拍板 Q；**不**预承诺
> 实现细节，**不**预设 commit 节奏，**不**改 round 1 trait 形态。
>
> reviewer 在 v0.4_entry §3.3 拍板候选 2 拆两轮 reviewer round；本
> CodeMap 是 round 2 入口前置工件。round 2 范围 / 触发条件 / 接班 10
> 步 checklist 见
> [`docs/handoffs/v0.4_robot_kinematics_round1_summary.md`](../../docs/handoffs/v0.4_robot_kinematics_round1_summary.md)
> §6。
>
> 写于 2026-05-19，commit `f9235e2`（候选 2 round 1 summary 归档）
> 推到 `origin/master` 之后。

---

## §0 TL;DR

候选 2 **round 1**（commit `6caf0ee`）已 land `crates/robot-kinematics/`
trait crate first release（`ArmIkSolver: Send + Sync` + 5 共享类型 +
`IkError` 5 variant + 4 lib unit；零 runtime / 零 wire / 零 robot-s1
改动）。round 2 把 trait 接进真路径：

| 触点 | round 2 实施期改动 | 现有锚点 |
|---|---|---|
| `crates/robot-kinematics-xlerobot/` | **新** workspace member；implement `ArmIkSolver` for dimos `arm_ik.py` 闭式 5-DOF | round 1 trait crate first release（`6caf0ee`）+ `Cargo.toml:26` "未来 crate" 注释段已预留 |
| `crates/robot-s1/src/action_backend/ik.rs` | **新** `IkBackend` 第三个真 action backend（沿 `HoldPoseBackend` / `ScriptedBackend` 同款模板）| `crates/robot-s1/src/action_backend/{hold_pose,scripted,mod}.rs` |
| `crates/robot-s1/src/coordinator.rs::Coordinator` | **新** 字段持 IK solver（reviewer 入口锚定 `Option<Box<dyn ArmIkSolver>>` 默认候选；如未来跨线程共享则备选 `Option<Arc<dyn ArmIkSolver>>`；待 Q-IK3 拍板）；`new()` / `submit()` / `Backing` 路径 | `coordinator.rs:1310-1322` Coordinator 字段；`:1334-1351` new() |
| `crates/robot-msg/schemas/ik_payload.fbs` | **新** `IkPayload` fbs body（字段集待 Q4 拍板）| `scripted_payload.fbs` round 1 候选 1 templates |
| schema 注册链 6 步 | `SCHEMA_IK_PAYLOAD` 注册（**下一空位 = `0x0000_040C`**，已验证；详 §3.4）+ `build.rs` 数组 13→14 + `markers.rs` BodyMarker impl + `size_tier.rs` arm + （可选）`build_helpers.rs` builder | `schema_id.rs:106` + `markers.rs:75-100` + `size_tier.rs:62-97` + `build.rs:57` |
| `ActionKindProto::ActionKindIk` (proto `ACTION_KIND_IK = 4`) / `ActionKindFb::Ik = 4` / `ActionKind::Ik` | 三层 enum append + 下游映射 atomic（沿候选 1 reviewer Plan round 1 阻断 1 锁定模式；prost 生成名沿 `ActionKindProto::ActionKindScripted` 同款 CamelCase + enum prefix）| `crates/robot-rpc/proto/robot_control.proto:119-124` + `action_request.fbs:38-43` + `action.rs:148-162` + `task_handlers.rs:1387-1394` + `rr_server/handler.rs:178-181` |
| `crates/robot-rpc/src/task_handlers.rs` | dispatch 映射扩 + 可能加 gateway 层 IK payload byte cap（候选 1 同款 `MAX_*_PAYLOAD_BYTES`；待 Q5 拍板）| `task_handlers.rs:1387-1394` + 候选 1 `MAX_SCRIPTED_PAYLOAD_BYTES` 模板 |
| 错误映射 | `IkError` 5 variant → `ActionFailure` / `ActionStartError` / `ActionRpcStatus` 映射（待 Q6 拍板）| `action.rs:494-547` + `action_status.fbs:83-91` + `action_status.fbs:124-134` + `task_handlers.rs:1432-1437` |
| `crates/robot-tests/src/bin/ik_backend_smoke.rs` | **新** cross-process e2e binary（沿 `scripted_backend_smoke` / `pubsub_cleanup_smoke` / `clear_estop_e2e` 三份样板）| `crates/robot-tests/Cargo.toml:94-100` `[[bin]]` 段 + 候选 1 + 3 模板 |
| `configs/` 新 IK profile TOML（可选） | XLeRobot 默认 `[s1.profile.backends] enabled = ["ik", ...]` + IK solver 选型字段（如 `[kinematics] xlerobot`）| `configs/scripted_backend_smoke.toml` round 1 候选 1 模板 |

**核心评估**（详 §2 + §4）：dimos `arm_ik.py` 5-DOF 闭式解 = 294 L
纯 `math` 库；移植到 Rust `robot-kinematics-xlerobot` 实施期容易；但
**round 1 trait 对接 dimos 闭式解有 3 处适配 gap**：
- dimos `solve()` 接 `(target_x, target_y, target_z, target_pitch, target_roll)` 5 标量 + 肩基座 frame；trait 接 `target: &Pose`（含 7-element xyz + xyzw quat）+ 隐式 frame —— **frame 抽象** + **5-DOF vs 6-DOF** mismatch
- dimos `solve()` **无 seed** + **无异常**（out-of-reach fallback clamp）；trait 接 `seed: Option<&[f64]>` + 返 `Result<..., IkError>` —— solver 实现得自主决定 clamp vs `Err(OutOfReach)`（AC-6 round 1 锚句的实测点）
- dimos 关节名 `shoulder_pan / shoulder_lift / elbow_flex / wrist_flex / wrist_roll` 跟 XLeRobot URDF / hardware adapter `joints` 顺序可能不一致 —— **关节名一致性 / 重映射策略 round 2 必审**（具体策略：solver 内部硬编码顺序 / `Coordinator::new()` 期校验 / `IkBackend::start()` 期延迟校验 等推到 Spec 拍板）

**预期 Q 数量**：~10-15（含 reviewer 列的 7 + 我列的 8+）。

**节奏决策**：reviewer 拍板 "round 2 是否需要先 land robot-
kinematics-xlerobot crate 再 land S1 wiring，CodeMap 只分析不要提前
定 commit 节奏" —— 本 CodeMap **不**预设 commit 数 / 拆段顺序 / 单
vs 多 commit；具体节奏推到 round 2 Plan 阶段（详 §7）。

---

## §1 round 1 trait 现状（**不**改）

`crates/robot-kinematics/src/lib.rs:135-165`：

```rust
pub trait ArmIkSolver: Send + Sync {
    fn info(&self) -> &KinematicInfo;
    fn solve(
        &self,
        target: &Pose,
        seed: Option<&[f64]>,
        deadline_ns: Option<u64>,
    ) -> Result<IkSolution, IkError>;
}
```

### §1.1 round 1 锁定的语义边界（round 2 必须沿用）

| 锚点 | round 1 锁定 | round 2 影响 |
|---|---|---|
| dyn-safe | `assert_dyn_safe::<dyn ArmIkSolver>()` 编译期 monomorphize（`lib.rs:267-278`）| S1 coordinator 持 `dyn ArmIkSolver`；round 2 实现 `XLeRobotArmIkSolver: ArmIkSolver` 必须保 dyn-safe |
| `IkError` 5 variant | `InvalidInput { reason }` / `NoSolution` / `OutOfReach { distance_m }` / `SingularityTooClose { condition_number }` / `DeadlineExceeded`（`lib.rs:95-110`）| round 2 solver 实现选择**哪些** variant 返回；如发现 5 不够走单独 reviewer round 扩 variant，**不**在 round 2 暗改 |
| 不预钉 clamp 行为 | 模块 doc `lib.rs:9-16` + trait doc `lib.rs:127-134` + Cargo.toml `description` + Spec AC-6 四处双写 | round 2 `XLeRobotArmIkSolver` impl **必须**显式声明：选 `Ok(clamped)` 还是 `Err(OutOfReach)` —— `arm_ik.py` 现有行为是 fallback clamp（详 §2 + Q6）|
| `deadline_ns: Option<u64>` | 对齐 `robot-core::time::MonoRawNs(u64)` + `ActionRequest.deadline_ns: u64`（`action.rs:242`）| round 2 S1 wiring 直接 `solver.solve(target, seed, Some(req.deadline_ns))` 透传，**无** cast |
| 零 runtime dep | `cargo tree --edges normal,no-proc-macro = {thiserror}`（round 1 summary §4.2）| round 2 trait crate `robot-kinematics` 本身 **0** 改 dep；可在 `robot-kinematics-xlerobot` impl crate 内 link `nalgebra` / vendor URDF（如需），不污染 trait crate |
| `KinematicInfo` 字段 | `joint_count: usize / joint_names: Vec<String> / joint_limits: Vec<JointLimit>`（`lib.rs:67-71`）| round 2 `XLeRobotArmIkSolver::info()` 必须填齐；关节名顺序由 solver 自决（trait 不约束）|

### §1.2 round 1 没解决但 round 2 必触的（Q11-Q20 round 2 范围）

round 1 Spec §"Reviewer round 1 拍板表" 把 Q11-Q20 推到 round 2：
- Q11 trait 是否扩 `solve_delta(current_angles)` / FK / Jacobian
- Q12 `IkPayload` fbs body 字段集（含 frame 抽象 / pose 形态 /
  tolerance / seed）
- Q13 IK 路径 cross-process e2e harness 模式
- Q14 XLeRobot clamp 行为选型（沿 dimos vs 拒）
- Q15 关节名重映射
- Q16-Q20 reviewer 推到 round 2 的其它问题（具体见 round 1 CodeMap
  `team-swe/codemap/v0_4_robot_kinematics.md` Q 列表）

**本 CodeMap §4 给的 Q1-Qn 是 round 2 自己列的 Q**，跟 round 1 Q11-Q20
有重叠 + 增量。round 2 Spec 阶段把两份 Q 合并去重 + reviewer 拍板。

---

## §2 dimos `arm_ik.py` 探查结果（适配评估，**不**预锁实现）

源文件：`/home/loosqk/repo/robot/dimos-deploy/src/xlerobot_dimos/
kinematics/arm_ik.py`（294 L）

### §2.1 关键事实

| 项 | dimos 现状 | round 1 trait 期望 |
|---|---|---|
| 解法类型 | **闭式** 5-DOF；2-DOF 平面 IK 用余弦定理 + 几何法 + wrist_roll 平凡映射 | trait **不**约束（数值 / 闭式都 OK）|
| 关节数 | **5-DOF**：`shoulder_pan` / `shoulder_lift` / `elbow_flex` / `wrist_flex` / `wrist_roll` | trait `KinematicInfo.joint_count: usize` 任意；XLeRobot impl 填 5 |
| 输入 | 5 标量：`solve(target_x, target_y, target_z, target_pitch=0, target_roll=0)` —— 笛卡尔 xyz + RPY 部分（pitch + roll，**不含** yaw；肩基座 frame） | trait `target: &Pose`（xyz + 4-element xyzw quat）+ **无 frame 参数** |
| seed | **无**（不接受初值；闭式独立解）| trait `seed: Option<&[f64]>`（数值法用，闭式 ignore）|
| 输出 | `ArmAngles(NamedTuple)` 5 字段 | trait `IkSolution { joints_rad: Vec<f64> }` |
| out-of-reach 处理 | **缩放降级 + clamp**（line 117-126 强制缩放到工作空间边界；line 143-144 / 242 / 245 关节限位 `.clamp()` 裁剪）；**无** exception | trait `IkError::OutOfReach { distance_m }` **可选**返回（AC-6 round 1 锚句）|
| singular 处理 | dimos **无** 显式 singular 检测 | trait `IkError::SingularityTooClose { condition_number }` **可选** |
| 外部依赖 | 仅 `math` 标准库；**0** numpy / scipy / pinocchio / kinpy | trait crate 自己零依赖；XLeRobot impl 可 link `nalgebra`（不影响 trait crate）|
| 关键函数 | `_planar_ik()` (2-DOF) / `solve()` (5-DOF 主) / `forward()` (FK) / `solve_delta()` (增量；round 1 Q11 推 round 2 trait 是否扩) | trait 只接 `solve()`；`forward` / `solve_delta` 不在 round 1 范围 |

### §2.2 适配 gap 3 处（**Q4 + Q6 + Q15 范围**；CodeMap **不**预决）

#### gap-1：5-DOF dimos `(xyz + pitch + roll)` vs trait `&Pose (xyz + xyzw quat)`

`Pose.orientation` 是 4-element xyzw quat（round 1 lib.rs:32-34）；
dimos `solve()` 接 pitch + roll 标量。round 2 `XLeRobotArmIkSolver::
solve()` 实施期：
- 解 `Pose.orientation` quat → roll/pitch/yaw 欧拉角
- `pitch + roll` 对接 dimos 5-DOF 主解（5-DOF 手臂不可达任意 yaw
  约束）
- 调内部 dimos 风格 `_planar_ik(...)` 做 5-DOF 主解（2-DOF 平面 IK
  + 几何法）+ `wrist_roll` 平凡映射（dimos `arm_ik.py` line 240+ 用
  `target_roll` 直接赋；闭式整体仍是 5-DOF）
- **yaw 处理策略待 Q-IK6 拍板**：(a) `Err(IkError::InvalidInput
  { reason: "yaw not supported by 5-DOF arm" })` / (b) ignore（CodeMap
  **不**预决；详 §4 Q-IK6）

**Q**：yaw 不为 0 时返 `Err(IkError::InvalidInput)` 还是**忽略**？
（详 §4 Q-IK6）

#### gap-2：dimos 无 frame 参数 vs trait `&Pose` 隐式 frame

`Pose` 类型本身**不**带 frame 标识（round 1 Q12 推 round 2）。dimos
默认 frame = 肩基座（shoulder_base）；XLeRobot 实际场景可能要
base_link / arm_base / end_effector 等其它 frame。

**Q**：round 2 是否在 `IkPayload` fbs body 引入 `frame: string` 字段
+ solver 内部 frame 变换；还是 trait 层完全约定 "由调用方保证 target
in solver-specific frame"？（详 §4 Q-IK7 + §3.4 Q-IK4）

#### gap-3：dimos 关节名 vs XLeRobot URDF / hardware adapter

dimos 用 `shoulder_pan / shoulder_lift / elbow_flex / wrist_flex /
wrist_roll`；XLeRobot motor ID 顺序 / URDF 关节名（如 `Rotation`,
`Pitch`, `Elbow`, ...）可能不一致。`KinematicInfo.joint_names: Vec<
String>` 由 solver 实现填，但 IK 输出 `IkSolution.joints_rad: Vec<
f64>` 的**顺序**必须跟 hardware adapter `JointCommand.positions[]`
顺序一致才能直接喂 `build_joint_command(...)`（详 §3.3）。

**Q**：round 2 是否在 `KinematicInfo.joint_names` 跟 XLeRobot
`HardwareSection.joints[].name` **强制一致性校验**（`Coordinator::
new()` 期 fail-fast）；还是 `IkBackend::start()` 期延迟校验？（详 §4
Q-IK10）

---

## §3 round 2 触点 7 类（reviewer 列）

### §3.1 `robot-kinematics-xlerobot` 新 workspace member（**Q-IK1**：crate 边界）

- **现有锚点**：`Cargo.toml:26` "未来 crate" 注释段已预留
  `crates/robot-kinematics-xlerobot # v0.4+：XLeRobot 可选 IK/FK
  backend（若 profile 选择 IK）`（round 1 (1/1) `6caf0ee` 删的是
  `robot-kinematics` 自己的注释行，**未**删 xlerobot 行）
- **预期改动**：
  * 新 `crates/robot-kinematics-xlerobot/Cargo.toml`（`[package]`
    workspace 继承 + `[lints] workspace = true` + `[dependencies]
    robot-kinematics = { workspace = true }`）
  * 新 `crates/robot-kinematics-xlerobot/src/lib.rs`（`XLeRobotArmIk
    Solver` struct + `impl ArmIkSolver for XLeRobotArmIkSolver` +
    内部 `_planar_ik` / dimos 风格闭式解 Rust 移植 + N 个 lib unit
    含 FK round-trip / 边界点 / clamp 行为锚句）
  * `Cargo.toml` workspace `members` append + "未来 crate" 注释段
    同名行删除
- **未决 Q**：
  - **Q-IK1**：是否需要在 round 2 内同时 land `robot-kinematics-
    xlerobot` workspace dep + `robot-kinematics` workspace dep 双
    `workspace = true` 入口？（CodeMap 不预决；详 §4）
  - **Q-IK2**：crate 是否需要可选 `nalgebra` feature gate（dimos
    闭式解只用 `math` 标库，但 round 2 + Jacobian / FK 扩展可能需
    要）？（默认**否**沿 round 1 零依赖原则；Plan 阶段拍板）

### §3.2 `IkBackend` 第三个真 action backend（**Q-IK3 + Q-IK4**）

#### §3.2.a 模板锚点

沿 `crates/robot-s1/src/action_backend/{hold_pose,scripted}.rs`
同款模板（`HOLD_POSE_BACKEND` / `SCRIPTED_BACKEND` static `inventory::
submit!` 注册）。

`ActionBackend` trait `crates/robot-s1/src/action_backend/mod.rs:
432-456`：

```rust
pub trait ActionBackend: Sync + Send + 'static {
    fn name(&self) -> &'static str;
    fn supported_kinds(&self) -> &'static [ActionKind];
    fn supported_schemas(&self) -> &'static [u32];
    fn start(
        &self,
        request: &ActionRequest,
        ctx: &mut ActionContext<'_>,
    ) -> Result<Box<dyn ActionInstance>, ActionStartError>;
}
```

`ActionInstance` trait `action.rs:474-484`：
```rust
pub trait ActionInstance: Send {
    fn tick(&mut self, ctx: &mut ActionContext<'_>)
        -> Result<ActionTickOutput, ActionFailure>;
    fn cancel(&mut self, ctx: &mut ActionContext<'_>)
        -> Result<(), ActionCancelError>;
}
```

#### §3.2.b round 2 预期 IkBackend 结构

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
    fn start(&self, req: &ActionRequest, ctx: &mut ActionContext<'_>)
        -> Result<Box<dyn ActionInstance>, ActionStartError> {
        // 1. 拿 solver: ctx.solver.ok_or(Rejected("no solver wired"))?
        // 2. decode IkPayload (待 Q-IK4 字段集)
        // 3. solver.solve(target, seed, deadline_ns)
        //    → Err(IkError) → 映射到 ActionStartError（待 Q-IK8）
        // 4. 返回 IkInstance { target_joints, dwell? }
    }
}
```

#### §3.2.c 未决 Q

- **Q-IK3**：solver 接入 `ActionContext` 还是 `Coordinator` 持有？
  （详 §3.3 + §4）
- **Q-IK4**：IkBackend 是 **single-shot**（解一次 → 一帧
  `JointCommand` → `Completed(Success)`）还是 **trajectory-like**
  （解一次 → 多 tick 插值过去 + dwell）？两条路径差异巨大。沿候选
  1 `ScriptedBackend` trajectory 模板 vs single-shot 直接命令两选
  推到 Plan 阶段；CodeMap **不**预决
- **Q-IK5**：cancel 收敛模式 —— single-shot 路径下 `cancel()` 几
  乎不可能触发（IK solve + 一帧 publish 太快）；trajectory-like 路
  径沿 hold_pose / scripted 同款 `cancelled: bool` flag + 下次
  tick 返 `PartialSuccess { "cancelled" }`

### §3.3 S1 solver mount point + ownership / feature flag / fail-fast（**Q-IK3 + Q-IK10 + Q-IK11**）

#### §3.3.a 现有锚点

`crates/robot-s1/src/coordinator.rs:1310-1322` 现有 `Coordinator`
struct：

```rust
pub struct Coordinator {
    inner: Mutex<InnerState>,
    enabled_backends: Vec<String>,
    registry: BackendRegistry,
    config: CoordinatorConfig,
    hardware: HardwareSection,
    layout_id: u64,
    anchor_bias: Vec<f64>,
    // ... no solver field
}
```

`Coordinator::new()` `coordinator.rs:1334-1351` 接收 `config /
hardware / layout_id / anchor_bias / registry / enabled_backends`；
**无** solver 注入路径。`ActionContext` `action.rs:357-365` 现有：

```rust
pub struct ActionContext<'a> {
    pub clock: &'a dyn RobotClock,
    pub layout_id: u64,
    pub hardware: &'a HardwareSection,
    pub config: &'a CoordinatorConfig,
    pub anchor_bias: &'a [f64],
    pub command_seq: u64,
    // ... no solver field
}
```

#### §3.3.b 四条 ownership 候选（**Q-IK3** Plan 阶段拍板）

| 选项 | 形态 | 优势 | 劣势 |
|---|---|---|---|
| (a) `Coordinator::solver: Option<Box<dyn ArmIkSolver>>` + `ActionContext::solver: Option<&'a dyn ArmIkSolver>`（**reviewer 入口锚定默认候选**） | Coordinator 唯一持有 solver；`ActionContext` 借用 `&'a dyn` 不持权；single solver instance；启动期 `new()` 注入 | 最小默认形态；ownership 简单（`Box` 单一持有，跟 `Coordinator: Send` 兼容；不需要跨线程共享理由）；feature gate 容易；`ArmIkSolver: Send + Sync` 已锁 round 1 → `Box<dyn ArmIkSolver>` 自动 `Send`；borrow `&'a dyn` 也 `Sync` | 多臂（XLeRobot dual-arm）必须 round 3+ 扩；CodeMap 不预决多臂；如未来同 solver 跨多 task 真并发共享需要升 Arc |
| (b) `Coordinator::solver: Option<Arc<dyn ArmIkSolver>>` + `ActionContext::solver: Option<&'a dyn ArmIkSolver>` | 跟 (a) 同 borrow 模式，仅持有切 `Arc` | 跨 tick / 跨 thread 真共享时不需重新设计；`Arc::clone` 是 hot-path-safe（但 round 2 S1 单线程 tick loop **不**需要 tick 内 clone）| **需要明确"跨线程共享"理由**才能选 `Arc`（reviewer 锚句）；round 2 单 coordinator + 单 tick thread 场景下 `Box` 等价；过度抽象 |
| (c) `Coordinator::solvers: HashMap<ArmId, Box<dyn ArmIkSolver>>`（或 `Arc<dyn ...>`）+ `IkPayload.arm_id` 字段 dispatch | 多 solver instance；多臂友好 | 第二轮 round 1 拍板 Q4(X) "多 solver instance"；本路径直接落地 | round 2 实施期多写 1 层 dispatch；CodeMap **不**预决（多臂推未来 reviewer round 更稳；详 §5）|
| (d) `IkBackend` 自己持有 `static` solver（feature gate compile-in） | 不动 `Coordinator` / `ActionContext` | 改动小 | static 跟运行时配置不解耦；feature flag 切换 = 重编译；不符合 round 1 Q4(X) "运行时多 instance" 拍板精神 |

#### §3.3.c feature flag 模式

沿 `crates/robot-s1/Cargo.toml:18-34` 现有 `backend-mock-dwell /
backend-hold-pose / backend-scripted` 模板。round 2 候选 feature 名：

- `backend-ik`（单一 feature；启用 `IkBackend` + 默认 XLeRobot solver）
- `kinematics-xlerobot`（独立 feature；解耦 backend 启用 vs solver
  选型）
- 两 feature 组合（`backend-ik` + `kinematics-xlerobot`；前者
  feature `dep:robot-kinematics`，后者 `dep:robot-kinematics-xlerobot`）

**Q-IK11**：feature 是否细分到 `kinematics-<robot>` 维度（沿 round 1
Spec §"严禁改动" 注释段预留 `robot-kinematics-piper` 等多机器人）？
还是 round 2 只暴露 `backend-ik` 单 flag？

#### §3.3.d Startup fail-fast 策略

`coordinator.rs:1334-1351` 现有 `new()` **无** fail-fast；状态机走
`Booting → Idle`（binary 层 anchor read 后调 `mark_idle()`）。

round 2 IK solver 缺失时的策略候选（**Q-IK10**）：
| 策略 | 形态 | 影响 |
|---|---|---|
| (a) Solver 缺失 = `IkBackend` 不 register（feature off）= `BackendRegistry::dispatch` 返 `NoBackendForKind(Ik)` | 沿现有 `Unimplemented` 行为；不破现有契约 | round 2 e2e smoke 跑前提 = feature 必须 on |
| (b) Solver 缺失但 `IkBackend` register = `start()` 返 `ActionStartError::Rejected("no solver wired")` | 跟 (a) 同语义但 lazy；config error 不在 startup 暴露 | startup 静默；不喜欢 |
| (c) Solver 缺失 + feature on = `Coordinator::new()` fail-fast `Err(InitError::SolverMissing)` | 最严；config error 第一时间暴露 | 需扩 `Coordinator::new()` 签名；breaking change |

#### §3.3.e 关节名一致性校验（**Q-IK10 续**）

`KinematicInfo.joint_names: Vec<String>` vs `HardwareSection.joints[]
.name` 一致性 —— 两条路径：
- (a) `Coordinator::new()` 期校验 `solver.info().joint_names ==
  hardware.joints.iter().map(|j| j.name)` → fail-fast
- (b) `IkBackend::start()` 期校验 → `ActionStartError::Rejected
  ("joint name mismatch")`

CodeMap 倾向 (a) startup fail-fast（config 错误第一时间暴露）；Plan
阶段拍板。

### §3.4 `IkPayload` schema 注册链（**Q-IK4 + Q-IK7**）

#### §3.4.a `SCHEMA_IK_PAYLOAD = 0x0000_040C` 下一空位**已验证**

`crates/robot-msg/src/schema_id.rs` 当前 schema_id 顺序：

```
0x0000_0000 SCHEMA_UNSPECIFIED
0x0000_0001 SCHEMA_JOINT_LAYOUT
0x0000_0002 SCHEMA_JOINT_STATE
0x0000_0003 SCHEMA_JOINT_COMMAND
0x0000_0100 SCHEMA_TWIST
0x0000_0200 SCHEMA_IMAGE_HEADER
0x0000_0201 SCHEMA_POINTCLOUD_HEADER
0x0000_0202 SCHEMA_TENSOR_HEADER
0x0000_0300 SCHEMA_ESTOP_EVENT
0x0000_0301 SCHEMA_HEARTBEAT
0x0000_0400 SCHEMA_CLEAR_ESTOP_REQUEST
0x0000_0401 SCHEMA_CLEAR_ESTOP_RESPONSE
0x0000_0402 SCHEMA_INVOKE_TASK_REQUEST
0x0000_0403 SCHEMA_INVOKE_TASK_RESPONSE
0x0000_0404 SCHEMA_GET_TASK_STATUS_REQUEST
0x0000_0405 SCHEMA_GET_TASK_STATUS_RESPONSE
0x0000_0406 SCHEMA_CANCEL_TASK_REQUEST
0x0000_0407 SCHEMA_CANCEL_TASK_RESPONSE
0x0000_0408 SCHEMA_CLEAR_ERROR_REQUEST
0x0000_0409 SCHEMA_CLEAR_ERROR_RESPONSE
0x0000_040A SCHEMA_ACTION_EVENT_ENVELOPE
0x0000_040B SCHEMA_SCRIPTED_PAYLOAD  ← 当前最大
0x0000_040C ← ✅ round 2 IK_PAYLOAD 下个空位
```

`SCHEMA_REGISTRY` `schema_id.rs:128-159` 升序校验 + `schema_lint`
binary 启动验证（`schema_id.rs:181` "SCHEMA_REGISTRY not strictly
ascending"）；round 2 append `SCHEMA_IK_PAYLOAD = 0x0000_040C` +
`SCHEMA_REGISTRY` 数组 append（升序自动保）。

#### §3.4.b 6 步注册链（沿候选 1 同款）

| # | 文件 | 现有锚 | round 2 改动 |
|---|---|---|---|
| 1 | `crates/robot-msg/schemas/ik_payload.fbs`（新）| `scripted_payload.fbs` 模板（`robot.msg` namespace）| 字段集**待 Q-IK4 拍板**；最小集候选见 §3.4.c |
| 2 | `crates/robot-msg/build.rs:57` `current_schemas: [&str; 13]`| 当前数组含 13 .fbs | → `[&str; 14]` + append `"ik_payload.fbs"` |
| 3 | `crates/robot-msg/src/schema_id.rs` | `SCHEMA_SCRIPTED_PAYLOAD = 0x040B`（line 106）+ `SCHEMA_REGISTRY` 当前 **21 条**（`SCHEMA_UNSPECIFIED` 有 const 但**不**进 registry）| append `SCHEMA_IK_PAYLOAD = 0x0000_040C` + `SCHEMA_REGISTRY` 升序末尾 entry → append 后 **22 条** |
| 4 | `crates/robot-msg/src/markers.rs` | `JointState` impl 模板（line 75-100）| 新 `pub struct IkPayload;` + `impl BodyMarker for IkPayload { SCHEMA_ID = SCHEMA_IK_PAYLOAD; SCHEMA_NAME = "robot.msg.IkPayload"; BLOB_EMBEDDED = false; type Root<'a> = msg::IkPayload<'a>; }` |
| 5 | `crates/robot-msg/src/size_tier.rs:62-97` `default_tier_of` match arm | `Control` 段含 `SCHEMA_SCRIPTED_PAYLOAD`（line 95）| → 同 `Control` arm 加 `SCHEMA_IK_PAYLOAD`（CodeMap **倾向** `Control` 8 KiB —— 单 pose payload ≤ 100 B 远低于 8 KiB；Spec / Plan 阶段最终拍板；详 Q-IK5）|
| 6 | `build_helpers.rs` builder（**可选**）| 候选 1 拍板 (a) inline 复制 → 跳过 | round 2 Plan 阶段拍板（**不**预决；沿候选 1 同款 inline 模式倾向）|

#### §3.4.c `IkPayload` 字段集候选（**Q-IK4 + Q-IK6 + Q-IK7**）

**最小集**（CodeMap 倾向但 round 2 Plan 拍板）：

```fbs
namespace robot.msg;

table IkPayload {
  // 目标 pose (xyz + xyzw quat)
  target_xyz: [double];              // 长度 == 3
  target_quat_xyzw: [double];        // 长度 == 4
  // (可选) seed 当前关节角；数值法 solver 用，闭式 ignore
  seed_joints_rad: [double];         // 空 = 无 seed
  // (可选) frame 标识；trait 不约束，solver 实现自决
  frame: string;                     // 空 = solver default
}

root_type IkPayload;
```

扩展集候选（待 Q-IK4 拍板**是否纳入 round 2**）：

| 字段 | 用途 | round 2 Plan 倾向 |
|---|---|---|
| `tolerance_pos_m: double` | 位置容差（数值 IK 用）| **不纳**（dimos 闭式解不用；XLeRobot impl 不需要）|
| `tolerance_rot_rad: double` | 姿态容差 | **不纳**（同上）|
| `arm_id: uint32` | 多臂 dispatch（双臂场景）| **延后**（Q4(X) round 1 拍板 "多 instance"，但 round 2 单臂先落；多臂走未来 reviewer round）|
| `priority: uint8` / `on_timeout: ubyte` | 沿 `JointCommand` 现有字段 | **不纳**（IK 解出来后走 `build_joint_command` 用 `CoordinatorConfig.priority` / `on_timeout`）|
| `max_segment_duration_ms: uint32` | IkBackend trajectory-like 期插值时长（Q-IK4 子问题）| **待 Q-IK4 拍板**（single-shot vs trajectory）|

#### §3.4.d 未决 Q

- **Q-IK4**：`IkPayload` 字段最小集 vs 扩展集？
- **Q-IK5**：`IkPayload` size_tier 落 `Control` (8 KiB) 还是
  `SensorSmall` (1 MiB)？最小集 ~50-100 B；`Control` 显然够 →
  CodeMap 倾向 `Control`（Plan 阶段确认）
- **Q-IK6**：5-DOF dimos 接 quat 但只用 pitch + roll —— yaw 不为 0
  时返 `Err(InvalidInput)` vs 忽略？
- **Q-IK7**：`IkPayload.frame: string` 字段是否纳入 round 2，还是
  trait 层完全约定 "由调用方保证 target in solver-specific frame"？

### §3.5 三层 `ActionKind::Ik` enum append（沿候选 1 atomic land 模式）

#### §3.5.a 现有三层（**= 4 variant Unknown/Unspecified 起算，3 个真 variant**）

```
ActionKindProto (crates/robot-rpc/proto/robot_control.proto:119-124)
  ACTION_KIND_UNSPECIFIED = 0   prost: ActionKindProto::ActionKindUnspecified
  ACTION_KIND_DWELL_MS    = 1   prost: ActionKindProto::ActionKindDwellMs
  ACTION_KIND_HOLD_POSE   = 2   prost: ActionKindProto::ActionKindHoldPose
  ACTION_KIND_SCRIPTED    = 3   prost: ActionKindProto::ActionKindScripted
                          ← round 2 ACTION_KIND_IK = 4 append
                            prost: ActionKindProto::ActionKindIk

ActionKindFb (action_request.fbs:38-43)
  Unknown    = 0
  DwellMs    = 1
  HoldPose   = 2
  Scripted   = 3
                          ← round 2 Ik = 4 append

ActionKind (action.rs:148-162)
  DwellMs / HoldPose / Scripted（Rust enum, 无 Unknown 等价）
                          ← round 2 Ik append
```

#### §3.5.b 下游映射（必须同 commit atomic land；候选 1 reviewer Plan round 1 阻断 1 模式）

| # | 文件 | 现有 | round 2 改动 |
|---|---|---|---|
| 1 | `task_handlers.rs:1387-1394` `action_kind_proto_to_fb` | 4 arm（含 Unspecified → Unknown）| add `ActionKindProto::ActionKindIk => ActionKindFb::Ik` |
| 2 | `task_handlers.rs` `action_kind_proto_to_fb` variant count 锁测（line 1546 `EXPECTED_VARIANT_COUNT: usize = 3`）| 当前 3（Dwell/Hold/Scripted）| → 4（+ Ik）|
| 3 | `rr_server/handler.rs:178-181` `ActionKindFb → ActionKind` 映射 | 3 arm + Unknown error | add `ActionKindFb::Ik => ActionKind::Ik` |
| 4 | `action.rs::ActionKind` `action_kind_three_v04_variants` 测（候选 1 land 的 3 variant 锁测）| HashSet 长度断言 3 | → 4 variant + 测名 `action_kind_four_v04_variants`（或保留旧名 + adjust）|
| 5 | `rr_server/handler.rs:645+` `ActionKindFb → ActionKind` exhaustive variant count 锁测（line 645 round 1 锁测） | 当前 3 | → 4 |

#### §3.5.c 未决 Q

- **Q-IK8**：candidate 1 (1/3) reviewer Plan round 1 阻断 1 锁定的
  "enum append + dispatch 映射必须 atomic 一个 commit" 是否在 round
  2 沿用？CodeMap **倾向沿用**（候选 1 学的教训）；Plan 阶段确认

### §3.6 IK fail → `ActionFailure` / `ActionStartError` / `ActionRpcStatus` 映射边界（**Q-IK8 + Q-IK9**）

#### §3.6.a 现有错误模型

`crates/robot-s1/src/action.rs:494-547`：

```rust
pub enum SubmitError {
    InvalidRequest(String),
    NoBackendForKind(ActionKind),
    UnsupportedSchema(String, u32),
    BusyExecuting(ActionId),
    CoordinatorDegraded,
    ShuttingDown,
    NotReady,
    BackendStartFailed(#[from] ActionStartError),
}

pub enum ActionStartError {
    Rejected(String),
    PayloadDecode(String),
    HardwareMismatch(String),
}

pub enum ActionFailure {
    DeadlineExceeded,
    PreconditionFailed(String),
    AdapterError(String),
    SafetyAbort(String),
    InternalError(String),
}
```

`crates/robot-msg/schemas/action_status.fbs:83-91 + 124-134`：

```fbs
enum ActionFailureKindFb : ubyte {
    Unknown=0, DeadlineExceeded=1, PreconditionFailed=2,
    AdapterError=3, SafetyAbort=4, InternalError=5,
}

enum ActionRpcStatus : ubyte {
    Unknown=0, Ok=1, FailedPrecondition=2, PermissionDenied=3,
    InvalidArgument=4, Internal=5, AlreadyExists=6,
    Unimplemented=7, NotFound=8, Unavailable=9,
}
```

`crates/robot-rpc/src/task_handlers.rs:1432-1437` 映射：
```rust
ActionFailureKindFb::DeadlineExceeded => P::DeadlineExceeded,
ActionFailureKindFb::PreconditionFailed => P::PreconditionFailed,
ActionFailureKindFb::AdapterError => P::AdapterError,
ActionFailureKindFb::SafetyAbort => P::SafetyAbort,
ActionFailureKindFb::InternalError => P::InternalError,
```

#### §3.6.b `IkError` 5 variant → 上层错误映射候选（**Q-IK8**）

| `IkError` | 候选映射 | 备选 | 备注 |
|---|---|---|---|
| `InvalidInput { reason }` | `ActionStartError::PayloadDecode(reason)` → gateway `InvalidArgument` | `ActionStartError::Rejected(reason)` → gateway `FailedPrecondition` | dimos 闭式解的 "yaw ≠ 0" / "seed 长度错" 等校验失败；倾向 `Rejected → FailedPrecondition` 沿候选 1 ScriptedBackend semantic check 同款 |
| `NoSolution` | `ActionStartError::Rejected("no solution")` → `FailedPrecondition` | `ActionFailure::PreconditionFailed`（tick 期 fail；但 IK 在 `start()` 期解，不在 tick）| 闭式解失败发生在 `start()`；倾向 `Rejected` |
| `OutOfReach { distance_m }` | **看 AC-6** —— XLeRobot solver 选 `Ok(clamped)` 路径就**不**会冒泡到 backend；选 `Err(OutOfReach)` 路径 → `ActionStartError::Rejected("out of reach: {distance_m:.4} m")` → `FailedPrecondition` | 跟 `NoSolution` 合并 | round 2 `XLeRobotArmIkSolver` impl 在 Plan 阶段确认走哪条 |
| `SingularityTooClose { condition_number }` | `ActionStartError::Rejected("near singular: cond={cond:.3e}")` → `FailedPrecondition` | `ActionFailure::SafetyAbort(...)` | dimos 闭式解**无** singular 检测；可能根本不会出现；放 `Rejected` 安全 |
| `DeadlineExceeded` | `ActionStartError::Rejected("solve deadline exceeded")` → `FailedPrecondition` | `ActionFailure::DeadlineExceeded` → `DeadlineExceeded` | 闭式解 O(1)，几乎不会超 ms 级 deadline；标记 placeholder |

#### §3.6.c 未决 Q

- **Q-IK8**：`IkError` 5 variant → `ActionStartError` 还是
  `ActionFailure`？两者落在不同阶段（start 期 vs tick 期）；闭式
  IK 解在 `start()` 期，倾向 `ActionStartError`，但 trajectory-like
  IkBackend（Q-IK4 选 trajectory 路径）可能 `solve()` 期分摊到 tick
  内（CodeMap **不**预决）
- **Q-IK9**：是否需要扩 `ActionFailureKindFb` 加 `IkSolverError`
  第 6 variant？还是 5 variant 够（沿现有 `PreconditionFailed`
  作 IK fail 兜底）？CodeMap **倾向 5 variant 够**（避免破 frozen
  schema 5 variant）；Plan 阶段确认。**如扩 variant 走独立
  reviewer round amend `action_status.fbs`**（破 frozen schema 边界
  需 reviewer 拍板）

### §3.7 `ik_backend_smoke` cross-process e2e（**Q-IK12 + Q-IK13**）

#### §3.7.a 模板锚点

`crates/robot-tests/Cargo.toml:94-100` 现有 `[[bin]]`：
- `pubsub_cleanup_smoke` (req `pubsub-cleanup-smoke`)
- `scripted_backend_smoke` (req `scripted-backend-smoke`)
- `clear_estop_e2e` (req `clear-estop-e2e`)
- `freeze_systemd_smoke` (req `freeze-systemd-smoke`)
- `crash_restart_smoke`（**无** required-features，本地 / Orin
  通用）

候选 1 / 3 模板（`scripted_backend_smoke.rs` 跟 `pubsub_cleanup_
smoke.rs`）共用 helper inline 复制（Q5(a) 候选 1 拍板路径 A 不抽
公共 lib）。

#### §3.7.b round 2 预期 `ik_backend_smoke` 结构

新文件：`crates/robot-tests/src/bin/ik_backend_smoke.rs`

CLI args（沿候选 1 同款）：
- `--bin-dir target/release`
- `--robot-id-prefix ik_backend`
- `--hardware-id smoke_arm`（required；沿候选 3 / 1 CLI 强校验）
- `--config configs/ik_backend_smoke.toml`（required；新 config TOML
  含 `[s1.profile.backends] enabled = ["ik", "hold_pose"]` +
  `[kinematics] xlerobot`）
- `--phase selftest | happy_single_target | source_task_id_monotonic | unreachable | invalid_input | all`
- `--output-csv <path>`（可选）

phase 候选（**Q-IK13**）：

| phase | 验证 | 沿 |
|---|---|---|
| `selftest` | harness 通 + fake S0 publish + IkPayload fbs build → root verify | 候选 1 / 3 同款 |
| `happy_single_target` | XLeRobot 工作空间内 pose → `InvokeTask` + Stream Started + ≥1 Progress + `Completed(Success)` 终态 + `JointCommand.positions[]` 跟 solver 解出 joints 一致 | 候选 1 happy_multi_waypoint 模板 |
| `source_task_id_monotonic` | 多 IK action 连续提交 → `JointCommand.source_task_id` 严格单调 | 候选 1 同款 AC-5 + B3 §3.7 |
| `unreachable` | 目标 pose 出工作空间 → 看 `XLeRobotArmIkSolver` 是 `Ok(clamped)` 还是 `Err(OutOfReach)`（详 Q-IK6 + AC-6 round 1 不预钉）；如 `Err` → terminal `Failed(PreconditionFailed)` | round 2 新 AC |
| `invalid_input` | NaN translation / 非规范化 quat / seed.len() != joint_count → terminal `Failed(PreconditionFailed)` 或 gateway `FailedPrecondition` | round 2 新 AC |

#### §3.7.c 未决 Q

- **Q-IK12**：`ik_backend_smoke` 跟 `scripted_backend_smoke` /
  `pubsub_cleanup_smoke` 三个 binary 共用 helper inline 复制（沿
  Q5(a) 路径 A） vs 抽公共 crate（候选 1 拒过的路径 B）？倾向继
  续 inline；Plan 阶段确认
- **Q-IK13**：cross-process e2e harness 是否在 `--phase
  unreachable` 实测 dimos clamp 行为，还是仅在 lib unit 内测
  `XLeRobotArmIkSolver::solve` 返回值类型？倾向 e2e 实测（端到端
  契约更强）；Plan 阶段确认

---

## §4 风险点 + Q 列表（reviewer 列的 7 + CodeMap 自列）

### §4.1 reviewer 列的 7 条风险点跟 Q 对应

| reviewer 风险点 | CodeMap Q | 处置原则 |
|---|---|---|
| 1. ArmIkSolver round 1 trait 是否足够承载 xlerobot 闭式解？不够只能列 Q，不能改 trait | **Q-IK6**（yaw 处理）+ **Q-IK7**（frame 字段）+ Q-IK9（错误 variant 扩）| trait 形态 **不**改 round 2；适配 gap 用 solver 实现 + IkPayload 字段 + 错误映射兜底；如**必须**改 trait 走独立 reviewer round amend round 1 |
| 2. IkPayload 的 frame / pose / tolerance / seed 字段定义，schema append 必须独立审 | **Q-IK4**（最小集 vs 扩展集）+ **Q-IK5**（size_tier）+ **Q-IK6**（yaw）+ **Q-IK7**（frame 字段）| round 2 Spec 阶段 reviewer round 1 拍板字段集；schema append atomic 沿候选 1 模式 |
| 3. SCHEMA_IK_PAYLOAD = 0x040C 是否真是下一个空位 | **已验证** —— §3.4.a 完整 schema_id 列表确认 0x040C 是 0x040B (Scripted) 后下一个；`schema_lint` 启动期升序校验自动锁 | 无 Q；事实锁定 |
| 4. S1 持 `Option<Box<dyn ArmIkSolver>>` 的 ownership / feature flag / startup fail-fast | **Q-IK3**（ownership 4 候选 a/b/c/d；(a) `Box<dyn>` reviewer 入口锚定默认 / (b) `Arc<dyn>` 备选需"跨线程共享"理由 / (c) `HashMap` 多臂 / (d) `IkBackend` static）+ **Q-IK10**（fail-fast 策略 a/b/c）+ **Q-IK11**（feature flag 粒度）| Plan 阶段 reviewer 拍板；CodeMap **不**预决 |
| 5. IkBackend 输出必须仍走 JointCommand path，不绕过 B3 source_task_id == ctx.command_seq 契约 | **Q-IK4**（single-shot vs trajectory）+ §3.2.b 预期结构强制经 `build_hold_command(..., ctx.command_seq, ...)` 或 `build_joint_command(..., source_task_id, ...)`（`coordinator.rs:362-378`）| **硬契约**：CodeMap 锁定 round 2 必须沿 B3 §3.7 + 候选 1 同款 `source_task_id == ctx.command_seq`；任何偏离 = 阻断 + 走 Plan-Execute diff（沿 B3 reviewer round 2 阻断 1 锁定模式）|
| 6. IK fail 映射到 ActionRpcStatus / ActionFailureKind 边界（unreachable / singular / invalid input / timeout）| **Q-IK8**（IkError → ActionStartError vs ActionFailure 哪一阶段）+ **Q-IK9**（是否扩 ActionFailureKindFb 第 6 variant）| Plan 阶段 reviewer 拍板；CodeMap §3.6.b 给映射候选表但**不**预决 |
| 7. round 2 是否需先 land robot-kinematics-xlerobot crate 再 land S1 wiring（CodeMap 只分析不要提前定 commit 节奏）| §7 commit 节奏 = **Plan 阶段拍板**；CodeMap **不**预决 | 沿 reviewer 拍板原则 |

### §4.2 CodeMap 自列的 Q（覆盖 round 2 但 reviewer 未直接列）

| Q | 内容 | 默认倾向（Plan 阶段确认）|
|---|---|---|
| **Q-IK1** | `robot-kinematics-xlerobot` workspace dep 入口（trait crate + impl crate 双 `workspace = true` 入口）| 倾向：仅 impl crate workspace dep；trait crate 已是 workspace member |
| **Q-IK2** | impl crate 是否启用 `nalgebra` 可选 feature | 倾向：**否**（沿 dimos 闭式解 + round 1 零依赖原则）|
| **Q-IK3** | Solver ownership：(a) `Coordinator::solver: Option<Box<dyn ArmIkSolver>>` + `ActionContext::solver: Option<&'a dyn ArmIkSolver>`（reviewer 入口锚定默认）vs (b) 同形态 `Arc<dyn>` 替 `Box<dyn>`（需要"跨线程共享" justification）vs (c) `HashMap<ArmId, Box<dyn>>` 多臂 vs (d) `IkBackend` 自持 static | 倾向 (a) `Option<Box<dyn ArmIkSolver>>` 单 solver（round 2 单臂先落，coordinator 单线程 tick loop 无需 Arc；多臂 Q4(X) 拍板生效推未来 reviewer round）|
| **Q-IK4** | `IkPayload` 字段最小集（target_xyz + target_quat_xyzw + seed_joints_rad + frame）vs 扩展集（+ tolerance + arm_id + ...）| 倾向：最小集 |
| **Q-IK5** | `SCHEMA_IK_PAYLOAD` size_tier | **倾向 `Control`**（单 pose payload ~50-100 B << 8 KiB）；Spec / Plan 阶段拍板，**未拍板前不锁**|
| **Q-IK6** | 5-DOF dimos：quat 含 yaw ≠ 0 时返 `Err(InvalidInput)` 还是忽略 | 倾向：返 `Err(InvalidInput("yaw not supported by 5-DOF arm"))` |
| **Q-IK7** | `IkPayload.frame: string` 字段是否纳入 round 2 | 倾向：纳入（占位空字符串）；实际 frame 变换 round 2 暂不做（solver 假设 shoulder_base frame）|
| **Q-IK8** | `IkError` → `ActionStartError` (start 期) vs `ActionFailure` (tick 期) | 倾向：闭式 IK 全 `start()` 期，全走 `ActionStartError` → `FailedPrecondition` |
| **Q-IK9** | 是否扩 `ActionFailureKindFb` 加第 6 variant `IkSolverError` | 倾向：**否**（沿现有 5 variant，避免破 frozen schema；走 `PreconditionFailed` 兜底）|
| **Q-IK10** | Startup fail-fast：solver 缺失 = no register / lazy fail / Coordinator::new fail | 倾向 (a) no register（feature off = `NoBackendForKind`）；最简 |
| **Q-IK11** | feature flag 粒度：`backend-ik` 单一 vs `backend-ik` + `kinematics-xlerobot` 双 vs `kinematics-<robot>` per-robot | 倾向：`backend-ik` 单一 default 进 + 默认 wired `XLeRobotArmIkSolver`（per-robot 多 solver 推未来 reviewer round）|
| **Q-IK12** | smoke helper inline 复制（沿候选 1 Q5(a)）vs 抽公共 crate | 倾向：inline（沿候选 1 / 3 同款）|
| **Q-IK13** | smoke phase 数量（5 phase 含 unreachable / invalid_input）| 倾向：5 phase（详 §3.7.b）|
| **Q-IK14** | round 2 是否需要 cargo deny check 同款 baseline 处理（沿候选 2 round 1 §P.4 diff #1）| 倾向：沿同款"未新增 violation；overall fail = baseline；follow-up 独立 commit" 措辞；**不**改 deny.toml |
| **Q-IK15** | round 2 是否触 §6.7 任一不变量（5 条）| 倾向：**0** 触（沿候选 1 + 候选 2 round 1 同款）|

---

## §5 不在 round 2 范围（→ 未来 reviewer round）

按 reviewer 强调"round 2 是预研，不预承诺实现" + round 1 Spec §"不在
范围"延伸：

- **`BaseKinematics` trait + `BaseKinematicInfo`** —— round 1 Q5(b)
  推到 round 2 但**仅作输入参考**；round 2 是否真 land 等 reviewer
  Spec 阶段拍板（CodeMap 倾向：跟 XLeRobot 三轮全向底盘运动学一
  起，留给 round 3 / 候选 4 (dimos 切换) 期；详 Q）
- **`robot-kinematics-piper` impl crate** —— 多机器人结构异构验证
  推到 v0.6+；workspace `Cargo.toml:27` 注释段保留
- **`solve_delta(current_angles)` / FK / Jacobian** trait 方法扩
  展 —— round 1 Q1(A) 最小 API 锁定；round 2 仅 `solve()` + `info()`
- **多臂 dispatch**（`IkPayload.arm_id` + `Coordinator::solvers:
  HashMap<ArmId, ...>`）—— round 1 Q4(X) 拍板"多 solver instance"
  但 round 2 单臂先落；多臂走未来 reviewer round
- **frame 抽象 / `FrameName` enum / `TransformTree`** —— round 1
  Q12 推 round 2；CodeMap 倾向 round 2 `IkPayload.frame: string`
  占位字符串 + 实际 frame 变换推未来 reviewer round
- **URDF 加载 / vendor URDF** —— round 1 Q14 推 round 2 选型；
  CodeMap 倾向 round 2 闭式硬编码（dimos 移植）+ URDF 推未来；如
  reviewer 选 URDF 路径走独立 reviewer round
- **真硬件回归 / Orin 真机 IK 端到端** —— round 2 land 后单独安排
  （沿 v0.3 B 真硬件回归节奏）
- **真 auth / mTLS / OIDC / rate limiting / circuit breaker** ——
  v0.5+
- **§6.7 5 不变量任一条降级 / 升级** —— **0** 触
- **`docs/00_design.md` §1.5 / §4.1 / §6 正文偏离** —— **0** 触；
  trait 形态 + 5 共享类型 + IkError 跟 §4.1 预写文件结构兼容；本
  round 2 内 schema append + ActionKind append + impl crate 落地 ==
  设计文档自然外推，**不**走 `docs/CHANGELOG.md` 偏离登记
- **`deny.toml` baseline 修复**（候选 2 round 1 §P.4 diff #1
  follow-up）—— 独立 commit / PR；**不**捆绑 round 2

---

## §6 跟其它 milestone handoff / spec 的链接

- [`docs/handoffs/v0.4_robot_kinematics_round1_summary.md`](../../docs/handoffs/v0.4_robot_kinematics_round1_summary.md)
  §3 关键契约审计（trait dyn-safe 编译期锁 / IkError exhaustive /
  不预钉 clamp / `deadline_ns: Option<u64>` 仓库时间对齐 / 零 runtime
  / 零 s1 改动 6 条；round 2 全部沿用，**0** 破）+ §6 候选 2 round 2
  入口 + §7 P.4 diff 5 项
- [`team-swe/codemap/v0_4_robot_kinematics.md`](v0_4_robot_kinematics.md)
  round 1 CodeMap（8 段 + Q1-Q20；Q1-Q10 已 round 1 ACK，Q11-Q20
  推 round 2 = 本 CodeMap §4 部分覆盖）
- [`team-swe/specs/v0_4_robot_kinematics.md`](../specs/v0_4_robot_kinematics.md)
  round 1 Spec round 1 + Plan v1 + reviewer Plan round 2/3 amend +
  AC-6 锚句 + §P.4 diff 5 项归档；round 2 Spec **新文件** 不在 round
  1 Spec 内续写（trait crate first release vs impl + wiring 两轮 reviewer
  round 边界清晰）
- [`docs/handoffs/v0.4_scripted_backend_summary.md`](../../docs/handoffs/v0.4_scripted_backend_summary.md)
  §3 关键契约审计（source_task_id 单调 / Dwell anchor / cancel 两
  层分离 / 两道 byte gate / ActionRpcStatus 映射 5 条；round 2
  IkBackend 沿用 1+2+3+5 四条；4 不触 = IK 单 payload 不需双 byte
  gate；详 Q-IK5）+ §6 候选 2 接班 checklist + §7 Plan-Execute
  diff 模板
- [`docs/handoffs/v0.4_pubsub_cleanup_summary.md`](../../docs/handoffs/v0.4_pubsub_cleanup_summary.md)
  §3 关键契约（reap_finished by `&self` / shutdown step 4 / Box::
  leak residual disclosure；round 2 **0** 触但回归门槛沿用）
- [`docs/handoffs/v0.3_b3_summary.md`](../../docs/handoffs/v0.3_b3_summary.md)
  §3 `source_task_id == ctx.command_seq` reasoning chain（round 2
  IkBackend 不可绕开；reviewer 风险点 5 锁定）
- [`docs/handoffs/v0.3_b2_outline.md`](../../docs/handoffs/v0.3_b2_outline.md)
  `ActionBackend` trait / `BackendRegistry` / `ActionContext` /
  `ActionTickOutput` 原始拍板表（round 2 IkBackend 沿 B2 trait 边
  界）
- `docs/00_design.md` §4.1 模块地图（`arm.rs` 预写文件结构）+
  §6.6.4 errata（IK 是可选 backend）+ §6.7 5 条不变量全保（round 2
  land 后预期仍 5/5）

---

## §7 round 2 节奏（**仅分析；不预设 commit 节奏**）

reviewer 显式要求："CodeMap 只分析不要提前定 commit 节奏"。本 §**不**
列单 commit / 2 commit / N commit 拆段倾向；Plan 阶段拍板。

### §7.1 round 2 RIPER 完整 5 阶段（沿候选 1 / 候选 2 round 1 同款）

| 阶段 | 工件 | reviewer round 数 | 触发 |
|---|---|---|---|
| **CodeMap**（本文件） | `team-swe/codemap/v0_4_robot_kinematics_round2.md` | 0+（reviewer 复审；如阻断 amend）| round 1 summary push (`f9235e2`) + reviewer round 2 entry ACK |
| **Spec** | `team-swe/specs/v0_4_robot_kinematics_round2.md`（新文件，**不**在 round 1 spec 内续写）| 1+（沿候选 1 Spec round 1+2 amend 模式）| CodeMap ACK + push |
| **Plan** | Spec 内 §P.0..P.4 续写（沿候选 1 / round 1 同款 in-file Plan）| 1+（Plan outline draft v1 + reviewer Plan round amend）| Spec ACK + push |
| **实施** | (1/n) ... (n/n) commit 链；commit 数 + 拆段顺序 **Plan 阶段拍板** | 每段 reviewer ACK | Plan Approved |
| **summary** | `docs/handoffs/v0.4_robot_kinematics_round2_summary.md`（新文件，沿候选 1 / round 1 同款模板）+ README anchor + CHANGELOG + CLAUDE.md 状态段 | 1+（reviewer 复审）| 最后一条实施 commit push |

### §7.2 round 2 实施期硬约束（CodeMap 锁定，Plan 阶段沿用）

| # | 约束 | 锚 |
|---|---|---|
| 1 | **不**改 round 1 trait 形态（`ArmIkSolver` 方法签名 / `IkError` 5 variant / 5 共享类型字段）；任何 trait 改动走独立 reviewer round amend round 1 | §1 + reviewer 风险点 1 |
| 2 | `IkBackend` 输出**必须**走 `build_hold_command(..., source_task_id, ...)` 或 `build_joint_command(..., source_task_id, ...)`；**不**绕过 B3 §3.7 `source_task_id == ctx.command_seq` 单调契约 | §3.2 + reviewer 风险点 5 |
| 3 | schema 注册链 6 步 atomic 一个 commit；enum 三层 append + dispatch 映射同 commit（沿候选 1 reviewer Plan round 1 阻断 1 锁定模式）| §3.4 + §3.5 + 候选 1 lessons learned |
| 4 | `SCHEMA_IK_PAYLOAD = 0x0000_040C` 已锁（schema_id 升序 + `schema_lint` 启动验证自动校验）；如 round 2 实施期发现冲突走 Plan-Execute diff 拍板新槽位 | §3.4.a |
| 5 | `XLeRobotArmIkSolver` impl 必须**显式**选 AC-6 clamp 行为路径（`Ok(clamped)` 或 `Err(OutOfReach)`）+ 4 处双写沿候选 2 round 1 同款模式（lib doc + Cargo.toml description + Spec AC + ik_payload.fbs comment？待 Plan 拍板）| §1 + §2.1 + Q-IK6 |
| 6 | `cargo deny check` 写"未新增 violation；overall fail = baseline；follow-up 独立 commit"措辞（**严禁**写"过"）；沿候选 2 round 1 §P.4 diff #1 同款 | §5 + Q-IK14 |
| 7 | §6.7 5 不变量 **0** 触；如 round 2 实施期发现潜在升级 / 降级走 Plan-Execute diff + reviewer 强 ACK | §5 + Q-IK15 |
| 8 | 重型 smoke 回归门槛升回 (a) **硬 gate**（`scripted_backend_smoke --phase all` + `pubsub_cleanup_smoke --phase all` 双进硬 gate；round 2 含 backend impl + S1 wiring + cross-process e2e）；round 1 拍 (b) compile gate + selftest 是零 runtime 代码路径特例 | round 1 summary §6 接班节奏第 6 步 |

### §7.3 round 2 实施期可能 follow-up 触发条件

- round 2 实施期发现 round 1 trait 不够装 XLeRobot 闭式解（Q-IK6
  / Q-IK7 拍板后实测撞 trait 边界）→ **必须**走独立 reviewer round
  amend round 1，**不**在 round 2 内暗改 trait
- round 2 实施期发现 `ActionFailureKindFb` 5 variant 不够（Q-IK9 拍
  板后实测撞 frozen schema 边界）→ 走独立 reviewer round amend
  `action_status.fbs`，**不**在 round 2 内暗扩
- round 2 实施期发现需要 multi-arm dispatch（Q-IK3 / Q4(X) round 1
  拍板"多 instance" 实测撞单臂 wiring）→ 走未来 reviewer round；
  round 2 单臂先落
- round 2 实施期发现 mock-s1 `crash_restart_smoke` baseline 回归
  （0/5 FAIL）→ 沿候选 1 mock-s1 baseline regression follow-up
  同款，**不**阻塞 round 2 任一 commit；§P.4 diff 登记

---

> **CodeMap 完结**：等 reviewer 复审 + ACK / 阻断 amend → Spec round 1
> 开（`team-swe/specs/v0_4_robot_kinematics_round2.md` 新文件）。
