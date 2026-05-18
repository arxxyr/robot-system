# CodeMap · v0.4 候选 2 — `robot-kinematics` trait + IK backend

> Pre-Research / CodeMap 输入（候选 2 第一轮 reviewer round 前置）。
> 沿 `v0_4_scripted_backend.md` / `v0_4_pubsub_cleanup.md` 同款模板：
> 当前挂点摸底 + 迁移源摸底 + 风险点穷举 + trait 草案候选方案。
> **本文件零拍板**：决策权全交 reviewer round 1 + Spec / Plan 阶段。
>
> 写于 2026-05-18，commit `11ac6aa` (候选 1 summary 归档) 之后。
> 候选 2 拆**两轮 reviewer round**（v0.4_entry §3.3 拍板）：
>
> - **第一轮**：`robot-kinematics` trait 草案 + 错误模型（**零** robot-
>   s1 实现改动；**零**具体机器人实现）
> - **第二轮**：`robot-kinematics-xlerobot` impl + S1 持 trait +
>   `IkBackend` + cross-process e2e + smoke
>
> 本 CodeMap **服务于第一轮**：让 reviewer 能在 trait 草案前看到
> robot-s1 / dimos-deploy / 设计文档三方边界 + 风险全貌。

---

## 0. TL;DR

候选 2 第一轮 round 钉 `crates/robot-kinematics/`（trait + 纯类型，
零 runtime 依赖）的**最小可用 API**。

**第一轮范围（硬）**：`ArmIkSolver` trait（笛卡尔 → 关节弧度）+ 共
享类型 `Pose` / `Twist` / `Jacobian` / `KinematicInfo` /
`IkSolution` / `IkError`。

**第一轮范围（待拍板）**：`BaseKinematics` trait 是否进第一轮 = **Q5
待拍板项**（§4.1 Q5；reviewer 倾向**未定**，必须 round 1 显式拍板；
Spec 阶段不得预设为第一轮必做，也不得预设为推后到第二轮）。

设计文档 §4.1 已预写文件结构（`arm.rs` / `base.rs` / `pose.rs` /
`error.rs`）+ 4 类 `IkError`，但**未**钉死方法签名、frame 抽象、
seed/timeout 字段、dyn-safety 选项；候选 2 第一轮 round 必须把这些
拍死。

第二轮 round 实施 `robot-kinematics-xlerobot`（移植 dimos
`arm_ik.py` 闭式解 + `units.py` 三轮全向）+ S1 `Option<Box<dyn
ArmIkSolver>>` 挂载 + 第三个真 action backend `IkBackend`（payload =
target pose + frame + tolerance；输出 joint position → 复用 B1
`build_hold_command` 路径）。第二轮 round 与候选 1 land 之间已硬序
（v0.4_entry §3.3）；候选 2 第一轮 trait 草案 round 可跟候选 1 实现期
重叠（不同 commit chain），但实际节奏候选 1 已 land（`70dcb25`
→ summary `11ac6aa`），候选 2 第一轮**现在**就可以开。

新 schema 预计 append（**不**钉死，第一轮 round 可推后到第二轮）：
`ActionKindProto::Ik = 4` + 新 `IkPayload` fbs body（schema_id 续号
`0x0000_040C`）+ `ActionKindFb::Ik = 4`。沿候选 1 同款 atomic
6-step 注册链 + 三层 ActionKind append（不动 B4 frozen schema）。

---

## 1. robot-system 当前挂点（候选 2 第一轮 round 必读）

### 1.1 `ActionBackend` / `ActionInstance` trait（B2 land；候选 1 复用）

**文件**：`crates/robot-s1/src/action.rs:432-485`

```rust
pub trait ActionBackend: Send + Sync {
    fn name(&self) -> &str;
    fn supported_kinds(&self) -> &[ActionKind];
    fn supported_schemas(&self) -> &[u32];
    fn start(
        &self,
        req: &ActionRequest,
        ctx: &ActionContext<'_>,
    ) -> Result<Box<dyn ActionInstance>, ActionStartError>;
}

pub trait ActionInstance: Send {
    fn tick(
        &mut self,
        ctx: &mut ActionContext<'_>,
    ) -> Result<ActionTickOutput, ActionFailure>;

    fn cancel(
        &mut self,
        ctx: &mut ActionContext<'_>,
    ) -> Result<(), ActionCancelError>;
}
```

**B2 reviewer round 锁定的硬边界**（候选 2 不解锁）：
- 不解锁 trait 方法签名（`fn start` / `fn tick` / `fn cancel` 三个签
  名不动；候选 2 `IkBackend` 必须沿用）
- 不解锁 enum variant 扩展边界（候选 2 append `ActionKind::Ik` 必须
  原子 atomic 跟 ActionKindProto / ActionKindFb / dispatch 映射 + 3
  variant count 锁测同 commit land，沿候选 1 (1/3) `d8ba207` 同款
  6-step pattern）
- `ActionContext` 6 字段不动（候选 1 land 后 = clock / layout_id /
  hardware / config / anchor_bias / command_seq；详 §1.2）

**候选 2 第二轮 round 内 IkBackend 接入路径**：
```rust
// 沿 ScriptedBackend / HoldPoseBackend 同款零字段 struct + inventory::submit!
pub struct IkBackend;
impl ActionBackend for IkBackend {
    fn supported_kinds(&self) -> &[ActionKind] { &[ActionKind::Ik] }
    fn supported_schemas(&self) -> &[u32] { &[SCHEMA_IK_PAYLOAD] }
    // ...
}
```

但 `IkBackend::start()` / `IkInstance::tick()` 内部**调** `ArmIkSolver`
trait（由 S1 持 `Option<Box<dyn ArmIkSolver>>` 注入，按 feature
flag 选具体实现）—— 候选 2 第一轮 round 必须钉死 trait 形态才能 IkBackend
实施时不返工。

### 1.2 `ActionContext` 字段集（候选 1 land 后）

**文件**：`crates/robot-s1/src/action.rs:352-359`

```rust
pub struct ActionContext<'a> {
    pub clock: &'a dyn RobotClock,
    pub layout_id: u64,
    pub hardware: &'a HardwareSection,
    pub config: &'a CoordinatorConfig,
    pub anchor_bias: &'a [f64],
    pub command_seq: u64,
}
```

候选 2 第二轮 round 内 `IkBackend` 拿 ctx 的方式（参考
ScriptedBackend）：
- `ctx.hardware.joints: Vec<String>` 拿关节名顺序（XLeRobot 5DOF
  arm 应该匹配 dimos `arm_ik.py` 的 `[shoulder_pan, shoulder_lift,
  elbow_flex, wrist_flex, wrist_roll]` 顺序，**但**dimos URDF 关节
  命名是 `[Rotation, Pitch, Elbow, Wrist_Pitch, Wrist_Roll]` ——
  存在 dimos arm_ik 内部 ↔ URDF 关节名重映射，候选 2 第二轮 round
  必须解决，第一轮 round 不预钉死）
- `ctx.anchor_bias: &[f64]` 拿当前 pose 作 IK seed / `solve_delta`
  基准（如 trait 有 seed 参数）
- `ctx.command_seq: u64` 透传 `source_task_id`（B3 §3.7 单调契约）
- `ctx.clock` 算 deadline / timeout

**候选 2 是否扩 `ActionContext` 字段？**：B3 reviewer round 2 § 3.4
显式放行"最小 lib API 扩展" 已经被候选 1 完全用掉（command_seq）；
候选 2 第一轮 round 倾向**不**扩 ActionContext 字段（IK solver 走
独立 trait object 注入，不进 ctx）；若实施时发现需要（如 IK 需要
`tool_frame: &TransformTree`），走 Spec / Plan 阶段重新评估 + 拍板。

### 1.3 `BackendRegistry::dispatch` 路径

**文件**：`crates/robot-s1/src/action_backend/mod.rs:90-194`

```rust
pub struct BackendRegistry {
    // `name() → &'static dyn ActionBackend`；inventory 注册时
    // `Sync + 'static` trait bound 保证字节静态（不是 Box；不是 owned String key）
    by_name: HashMap<&'static str, &'static dyn ActionBackend>,
}

pub fn dispatch(
    &self,
    enabled: &[String],
    kind: ActionKind,
    schema_id: u32,
) -> Result<&'static dyn ActionBackend, SubmitError>;
```

**失败语义**（`mod.rs:152-158` doc）：
- `enabled` 中没有任何 backend `supported_kinds()` 含 `kind` →
  `SubmitError::NoBackendForKind(kind)`
- `enabled` 中 N 个 backend 支持 `kind`，但其 `supported_schemas()`
  全都不含 `schema_id` → `SubmitError::UnsupportedSchema(backend_name,
  schema_id)`，`name` 字段填首个支持 `kind` 的 backend 名（debug 友好）

候选 2 第二轮 round 内 `IkBackend` 接入：仅扩 `inventory::submit!`
+ `[s1.profile.backends]` 加 "ik"。dispatch 路径**完全不动**（沿候
选 1 同款 atomic land）；`SubmitError` 两 variant 沿用，**不**新引
入 `DispatchError`。

### 1.4 现有 backend 接入对照（候选 2 第二轮 round 模板）

| 维度 | `HoldPoseBackend` (B3) | `ScriptedBackend` (候选 1) | `IkBackend` (候选 2 第二轮) |
|---|---|---|---|
| supported_kinds | `&[HoldPose]` | `&[Scripted]` | `&[Ik]` |
| supported_schemas | `&[NO_PAYLOAD_SCHEMA_ID]` | `&[SCHEMA_SCRIPTED_PAYLOAD]` | `&[SCHEMA_IK_PAYLOAD]` (新) |
| payload | 空 | `ScriptedPayload` (waypoints) | `IkPayload` (target pose + frame + tolerance) |
| tick 输出 | `build_hold_command(anchor_bias)` 复用 hold loop | per-tick lerp_per_joint → `build_hold_command(tick_buf)` | IK solver 求解一次 → 多 tick 复用 `build_hold_command(solution)` 或 → 类 Scripted 模式从 anchor pose 到 IK solution 做 joint-space 线性 ramp |
| cancel | cooperative flag | cooperative flag | cooperative flag |
| 状态机 | 单 phase | Interp / Dwell 双 phase | TBD（reviewer 第二轮 round 拍板：单 phase 直接 ramp vs Plan / Execute 双 phase） |
| feature gate | `backend-hold-pose` | `backend-scripted` | `backend-ik`（新） |

### 1.5 Cargo workspace feature 模式（候选 2 接入蓝本）

**文件**：`crates/robot-s1/Cargo.toml`

```toml
[features]
default            = ["backend-mock-dwell", "backend-hold-pose", "backend-scripted"]
backend-mock-dwell = []
backend-hold-pose  = []
backend-scripted   = []
```

每个 feature 守门 `#[cfg(feature = "...")] pub mod foo;`；文件不
link → `inventory::submit!` 收不到 → profile `[s1.profile.backends]`
启动期 fail-fast。

**候选 2 第二轮 round 扩展**（**伪码示意**；实际 dep activation 走
Cargo 2024 edition `dep:<crate>` 显式语法，第二轮 round 拍板具体
形态）：
- 加 `backend-ik = ["dep:robot-kinematics"]`（拉 trait crate 作
  optional dep；不自动启用任何 `robot-kinematics-<robot>` 实现）
- 加 `kinematics-xlerobot = ["backend-ik", "dep:robot-kinematics-
  xlerobot"]`（或类似 namespacing；具体形态 reviewer 第二轮 round
  拍板）
- `robot-kinematics-xlerobot` crate 自己**不**默认启用（XLeRobot 用
  户显式开 `kinematics-xlerobot` feature；Piper 等其他机器人换
  feature）

**workspace 蓝图已预留**（`Cargo.toml` 注释 + `CLAUDE.md` §2）：
```text
# 未来 crate（按 docs/01_implementation_steps.md 顺序逐步启用；现在不要提前加入 members）：
#   crates/robot-kinematics          # v0.3+：可选 IK/FK backend trait + 纯类型
#   crates/robot-kinematics-xlerobot # v0.4+：XLeRobot 可选 IK/FK backend
#   crates/robot-kinematics-piper    # v0.6+：Piper 可选 IK/FK backend
```

候选 2 第一轮 round 仅启用 `robot-kinematics`（trait crate；零
runtime 依赖；`#![no_std]` 候选 = 待 reviewer round 1 拍板）。

### 1.6 `docs/00_design.md` §4.1 + §6.6.4 边界（候选 2 必须遵守）

**§4.1 模块地图**（`docs/00_design.md:581-590`）已预写 `robot-
kinematics` crate 文件结构：

```text
robot-kinematics/    # 零 runtime 依赖；可选 kinematics trait 层
  ├─ arm.rs          # ArmIkSolver / KinematicInfo / IkSolution / IkError
  ├─ base.rs         # BaseKinematics（body↔wheel、里程计积分）
  ├─ pose.rs         # Pose / Twist / Jacobian 纯类型
  └─ error.rs        # NoSolution / OutOfReach / SingularityTooClose / DeadlineExceeded

robot-kinematics-<robot>/     # 每种机器人一个可选 IK/FK 实现
  ├─ xlerobot/                # XLeRobot 双臂 5DoF + 三轮全向（v0.4；移植自 dimos kinematics/）
  ├─ piper/                   # AgileX 单臂（v0.6，结构异构验证）
  └─ <user>/                  # 用户可 fork 自己的实现，不动 s1 / s0 / adapter
```

**§6.6.4 errata**（`docs/00_design.md:1168-1174`，2026-04-27 加）：
> IK/FK、RL、scripted controller 都是 action backend。v0.3 只要求至
> 少一个 backend 闭环到 S0 joint command；如果 RL 直接输出 joint
> target / joint delta / normalized action，则**不**需要 IK/FK。
> S0 / adapter **不**知道 action 来源，且 v0.3 不开放 raw torque /
> raw current policy 直通 S0。

**候选 2 推论**：
- IK 是**可选**功能 ——`robot-kinematics` 不进 `robot-core` /
  `robot-s0` 依赖；workspace 默认不 link
- 第二轮 round `IkBackend` 输出走**现有** `JointCommand` body 路径
  （`build_hold_command(cfg, layout_id, joint_positions, command_seq,
  now_ns)`），**不**绕开 S1 safety / joint limits / rate limit
- `IkError::SingularityTooClose` 等运行时 IK 错误 → 经
  `ActionFailure::PreconditionFailed` 路径 → coordinator 回 IDLE +
  hold loop 自恢复（**不**进 DEGRADED；详 B2 outline §6.5 fallback
  表）

### 1.7 §6.7 5 条不变量（候选 2 必须保持）

| # | 候选 2 影响 | 说明 |
|---|---|---|
| #1 S0 存活 = 软件层安全充分条件 | ✅ 不动 | IK 在 S1；S0 不知道 action 来源 |
| #2 systemd 看门狗 | ✅ 不动 | 候选 2 不触 |
| #3 EStop 不可清为失忆态 | ✅ 不动 | 候选 2 不触 latch 路径 |
| #4 S0 重启不失忆 | ✅ 不动 | crash_restart_smoke 沿候选 1 同款回归门槛 |
| #5 幂等写 RPC | ✅ 不动 | IK payload 走 InvokeTask + idempotency_key 标准路径 |

---

## 2. dimos-deploy 迁移源（候选 2 第二轮 round 实施期参考）

### 2.1 `arm_ik.py` 摘要（CodeMap 探查结果，逐条引用）

**文件**：`/home/loosqk/repo/robot/dimos-deploy/src/xlerobot_dimos/kinematics/arm_ik.py`

| 维度 | 内容 |
|---|---|
| 入口 API | `ArmIK(side: "left"\|"right").solve(target_x, target_y, target_z, target_pitch=0.0, target_roll=0.0) -> ArmAngles` （`arm_ik.py:173-253`） |
| 算法 | **闭式几何解**（纯 `math`，零 vendor IK 库）；`atan2` + 余弦定理 + 几何链接（`arm_ik.py:129-150`） |
| 依赖 | `pyproject.toml`：`numpy>=2.2`（IK 本身不用 numpy；仅其它模块）+ `opencv-python` + `rerun-sdk`；**IK 零外部依赖** |
| 关节顺序 | `ArmAngles` NamedTuple：`[shoulder_pan, shoulder_lift, elbow_flex, wrist_flex, wrist_roll]`（弧度） |
| Frame | 笛卡尔 `(x, y, z)` **相对肩基座** frame；右手系（X 前 / Y 左 / Z 上） |
| 错误模型 | **无显式错误处理**：工作空间外自动 clamp 到边界（`arm_ik.py:117-126`）；fail-safe 不抛异常 |
| seed / warm start | `solve()` **不需要**；`solve_delta(current_angles)` 需要（Quest 手柄实时控制场景） |
| URDF 加载 | **不加载** URDF；参数硬编码（`arm_ik.py:42-52`）：l1=0.1159m / l2=0.1350m / 肩高=0.0165m / 肩横偏=0.0452m（XLeRobot SO101Kinematics 标准 + 手工标定） |
| 关节名重映射 | dimos arm_ik 内部命名（`shoulder_pan` / ...）跟 URDF 命名（`Rotation` / `Pitch` / `Elbow` / `Wrist_Pitch` / `Wrist_Roll`）**不一致**；候选 2 第二轮 round 必须解决 |

**对候选 2 trait 草案的影响**：
- 闭式解 → 不需要 seed 参数（trait 接 `seed: Option<&[f64]>` 即可，
  闭式实现 ignore）
- 不抛异常 → trait 错误模型仍按 §4.1 4 类 IkError 设计，**但**
  XLeRobot 闭式实现按 dimos 行为 fail-safe clamp 而**不**返
  `OutOfReach`（reviewer 第一轮 round 拍板：trait 是否强制 backend
  返 error，还是允许 backend 自行 clamp + 加 informational metric）
- 5DOF + 单臂"side" 概念 → trait 需要 robot 多臂时区分 `Arm::Left` /
  `Arm::Right`（trait 形态：solver instance 持有 side state？还是
  方法参数？reviewer 第一轮 round 拍板）

### 2.2 `units.py` 三轮全向 base kinematics

**文件**：`/home/loosqk/repo/robot/dimos-deploy/src/xlerobot_dimos/common/units.py:75-136`

| 维度 | 内容 |
|---|---|
| 内容 | 三轮全向底盘运动学：body twist `(vx, vy, ω)` ↔ wheel rad/s `(w0, w1, w2)`；numpy 矩阵 K |
| API | forward-only mapping（body → wheel；wheel → body 是逆矩阵） |
| 错误处理 | 无（线性映射不可能撞 singularity） |
| 注 | 跟 arm IK 不同：base **没有** "目标 pose 解 wheel 速度" 的 IK 概念；只有"目标 body twist 解 wheel 速度"的运动学（forward kinematics 思路） |

**对候选 2 `BaseKinematics` trait 草案的影响**：
- `BaseKinematics` 只需要 forward 方法 `body_twist_to_wheel_rad_s` +
  逆向 `wheel_rad_s_to_body_twist`（里程计积分）
- **不**需要 `solve_pose_to_wheel_command`（base 没有"求解 wheel
  command 到达某 pose"问题；高层 task layer 自己分解 pose → twist
  序列）
- 设计文档 §4.1 `base.rs` 描述"body↔wheel、里程计积分" = 正反两个
  forward 方向；不含 "IK"

### 2.3 URDF 文件

**位置**：`/home/loosqk/repo/robot/dimos-deploy/assets/xlerobot/`
- `xlerobot.urdf`（双臂 5DOF + 三轮全向；含 mesh 引用）
- `xlerobot.srdf`（语义描述）
- `xlerobot_front.urdf`（前视图版本）

dimos `arm_ik.py` **不**加载 URDF（参数硬编码）；候选 2 第二轮 round
`robot-kinematics-xlerobot` 实现策略：
- 路径 A（最少依赖）：沿 dimos 硬编码 + arm_ik.py 移植；URDF 仅作文
  档/可视化参考；`robot-kinematics-xlerobot` 零 URDF 加载依赖
- 路径 B（更通用）：vendor URDF + parse；`robot-kinematics-xlerobot`
  加 `urchin` / `urdfpy` 等 dep；闭式实现读 URDF 参数（链接长度 /
  关节范围）做 self-check

reviewer 第二轮 round 拍板（**第一轮 round 不预钉死**）。

---

## 3. trait 草案候选方案（reviewer round 1 拍板表预备）

> 本节是 **draft option matrix**，候选 2 第一轮 reviewer round
> 的拍板输入；**不**是 spec 也不是 plan。

### 3.1 `ArmIkSolver` trait API 选型（3 候选）

#### 候选 A — 最小 API（`solve` 单方法 + 内置 metadata）

```rust
pub trait ArmIkSolver: Send + Sync {
    /// Arm 元信息：joint 数 / joint 名 / 关节范围。
    fn info(&self) -> &KinematicInfo;

    /// Cartesian → joint positions。
    ///
    /// - target：目标 pose（基座 frame）
    /// - seed：可选 seed（数值法用；闭式 ignore）
    /// - deadline_ns：可选 deadline；超过返 DeadlineExceeded
    fn solve(
        &self,
        target: &Pose,
        seed: Option<&[f64]>,
        deadline_ns: Option<i64>,
    ) -> Result<IkSolution, IkError>;
}
```

**优**：dyn-safe（无 generic / `Self` 返回）；最少决策面；闭式 / 数
值 / vendor 都能套；trait 不暴露 `&dyn RobotClock` （deadline_ns 是绝
对时间 from `RobotClock::now()`，trait 层只看数值）

**劣**：未来 `solve_delta(current_angles)` / FK / Jacobian 都要扩
trait 方法（trait 一旦 land 就难加新方法）

#### 候选 B — 全套 API（IK / FK / Jacobian / delta solve）

```rust
pub trait ArmIkSolver: Send + Sync {
    fn info(&self) -> &KinematicInfo;
    fn solve(&self, target: &Pose, seed: Option<&[f64]>, deadline_ns: Option<i64>) -> Result<IkSolution, IkError>;
    fn solve_delta(&self, target: &Pose, current: &[f64], deadline_ns: Option<i64>) -> Result<IkSolution, IkError>;
    fn forward(&self, joints: &[f64]) -> Result<Pose, IkError>;
    fn jacobian(&self, joints: &[f64]) -> Result<Jacobian, IkError>;
}
```

**优**：覆盖 dimos `arm_ik.py` 完整功能面（含 `solve_delta`）+ 雅可
比未来用于 velocity-level control / Cartesian impedance control

**劣**：决策面大；FK / Jacobian 在候选 2 第二轮 round 实施期可能用
不上（XLeRobot SO101 直接 IK 输出即可）；trait 越大越难 dyn-safe
通过

#### 候选 C — 最小 API + extension trait（hybrid）

```rust
pub trait ArmIkSolver: Send + Sync {
    fn info(&self) -> &KinematicInfo;
    fn solve(&self, target: &Pose, seed: Option<&[f64]>, deadline_ns: Option<i64>) -> Result<IkSolution, IkError>;
}

/// 可选：solver 想支持 delta solve / FK / Jacobian 时 impl 这个 trait。
pub trait ArmIkSolverExtended: ArmIkSolver {
    fn solve_delta(&self, target: &Pose, current: &[f64], deadline_ns: Option<i64>) -> Result<IkSolution, IkError>;
    fn forward(&self, joints: &[f64]) -> Result<Pose, IkError>;
    fn jacobian(&self, joints: &[f64]) -> Result<Jacobian, IkError>;
}
```

**优**：核心 trait 最小；扩展可选；S1 持 `Box<dyn ArmIkSolver>`，
高级特性走 downcast / TypeId 检测

**劣**：double trait 复杂度；S1 接入路径需要"已知 solver 类型"才能
拿到 extended 方法 —— 对动态 backend 选 solver 场景不友好

**reviewer round 1 倾向预测**：候选 A（最小 API）—— 跟 B2 `ActionBackend`
trait 同款"先钉最小可用，后续扩展走 reviewer round"模式（B3 ctx.command_seq
扩字段先例）。

### 3.2 `IkError` 错误模型选型（2 候选）

#### 候选 α — §4.1 已写的 4 类（保留）

```rust
pub enum IkError {
    NoSolution,
    OutOfReach { distance_m: f64 },
    SingularityTooClose { condition_number: f64 },
    DeadlineExceeded,
}
```

**优**：直接落 §4.1 预写；4 类语义清晰

**劣**：dimos `arm_ik.py` 是 fail-safe clamp 不抛异常 —— 候选 2 第二
轮 round 实施时 XLeRobot 实现要怎么映射？要么 backend 加 informational
metric 不返 error，要么改 dimos clamp 行为返 OutOfReach（行为差异；
reviewer 第二轮 round 拍板）

#### 候选 β — 加 `InvalidInput` + `Internal`

```rust
pub enum IkError {
    InvalidInput { reason: String },
    NoSolution,
    OutOfReach { distance_m: f64 },
    SingularityTooClose { condition_number: f64 },
    DeadlineExceeded,
    Internal { source: Box<dyn Error + Send + Sync> },  // dyn-safe issue?
}
```

**优**：覆盖更多场景（如 target pose NaN / quat 非规范化 / config 缺
失）

**劣**：`Box<dyn Error>` 在 trait 错误类型里破坏 `Send + Sync`
auto-impl；候选 2 第一轮 round 内**应避免**

**reviewer round 1 倾向预测**：候选 α（沿 §4.1 预写），加
`InvalidInput { reason: String }` 一个，覆盖运行时输入校验失败（NaN
/ quat 非规范化）。

### 3.3 `Pose` / `Twist` / `Jacobian` 类型选型（3 候选）

#### 候选 i — 自带类型（`#[no_std]` 友好）

```rust
pub struct Pose {
    pub translation: [f64; 3],  // x, y, z
    pub orientation: [f64; 4],  // quaternion x, y, z, w（normalize 保证调用方做）
}

pub struct Twist {
    pub linear: [f64; 3],
    pub angular: [f64; 3],
}

pub struct Jacobian {
    pub rows: usize,
    pub cols: usize,
    pub data: Vec<f64>,  // row-major; `rows == 6` for spatial Jacobian
}
```

**优**：零外部依赖（`robot-kinematics` 真零 runtime）；`Pose` /
`Twist` 是 POD 可在 `no_std` 上跑；Jacobian 可用 `Vec` 或换
`SmallVec<[f64; 36]>` 调

**劣**：`Jacobian` 没有矩阵运算（求逆 / SVD）—— 调用方自己实现 / 调
nalgebra；如果第二轮 round 实施时数值法需要矩阵运算，要么 backend
内部 link nalgebra，要么 trait 加 vendor 依赖

#### 候选 ii — nalgebra-based

```rust
use nalgebra::{Isometry3, Vector3, Vector6, OMatrix, Const, Dyn};

pub type Pose = Isometry3<f64>;
pub type Twist = Vector6<f64>;
pub type Jacobian = OMatrix<f64, Const<6>, Dyn>;
```

**优**：开箱即用矩阵运算；`Isometry3` 自动维护 quat normalization；
未来加 trajectory / impedance control 不返工

**劣**：`robot-kinematics` 引入 `nalgebra` runtime 依赖（违反 §4.1
"零 runtime 依赖" 设计原则）

#### 候选 iii — `nalgebra` optional feature

```rust
// 默认：自带 [f64; N] 类型
// optional: feature `nalgebra` 启用 Pose = Isometry3 等

#[cfg(feature = "nalgebra")]
pub type Pose = nalgebra::Isometry3<f64>;
#[cfg(not(feature = "nalgebra"))]
pub struct Pose { /* ... */ }
```

**优**：调用方按需启用；保持零必需依赖

**劣**：feature gate 让 trait 签名分裂（用 `nalgebra` 的 backend
跟不用的 backend 不能互换实现）；S1 接 `Box<dyn ArmIkSolver>` 时
trait 签名要在 ABI 层一致 → 实践上 feature 选定后整 workspace 必须
统一，**不**能真做到 per-backend optional

**reviewer round 1 倾向预测**：候选 i（自带类型，零依赖）—— 跟 §4.1
"零 runtime 依赖" 原则一致；XLeRobot 闭式实现不需要矩阵运算；如未来
数值法需要，在 `robot-kinematics-<robot>` 实现内部 link nalgebra 不
污染 trait crate。

### 3.4 多臂支持选型（XLeRobot 双臂场景；2 候选）

#### 候选 X — 多 solver instance（per-arm）

```rust
// S1 持 HashMap<ArmName, Box<dyn ArmIkSolver>>
// IkBackend payload 含 arm_name: String
// dispatch 时按 arm_name 选 solver
```

**优**：trait 不需要 side 字段；每臂独立 solver instance
（XLeRobot 左 / 右两个 ArmIK instance）

**劣**：S1 wiring 复杂度上升；feature flag 设计要支持"启 / 禁单臂"

#### 候选 Y — 单 solver，`solve` 方法接 arm 参数

```rust
pub trait ArmIkSolver {
    fn solve(&self, arm: ArmHandle, target: &Pose, ...) -> Result<IkSolution, IkError>;
}

pub enum ArmHandle {
    Single,
    Left,
    Right,
}
```

**优**：trait 暴露多臂语义；S1 只持一个 solver instance

**劣**：单臂机器人（Piper）也要带 `ArmHandle::Single` 参数，语义
冗余

**reviewer round 1 倾向预测**：候选 X（多 solver instance）—— 单臂
solver 是 trait 第一公民；多臂用容器 + dispatch；跟 dimos `ArmIK(side)`
模式一致（dimos 实际跑两个 `ArmIK("left")` / `ArmIK("right")`
instance）。

### 3.5 `BaseKinematics` trait 草案（最小候选）

```rust
pub trait BaseKinematics: Send + Sync {
    /// Base 元信息（轮子数 / 配置）。
    fn info(&self) -> &BaseKinematicInfo;

    /// body twist (linear vx/vy/vz + angular ωx/ωy/ωz) → wheel rad/s。
    fn forward(&self, body_twist: &Twist) -> WheelCommand;

    /// wheel rad/s → body twist（里程计积分用）。
    fn inverse(&self, wheel_rad_s: &[f64]) -> Twist;
}
```

**注**：跟 §4.1 `base.rs` "body↔wheel、里程计积分" 描述一致；
**不**含 "IK"（base 没有 pose-to-wheel-command 求解概念；高层 task
layer 自己分解 pose → twist 序列）。

**reviewer round 1 拍板项**：本节仅给候选 (a) 的 `BaseKinematics`
trait 草案；**是否**进入第一轮 round 由 Q5 显式拍板（详 §4.1 Q5；
两选项 (a) 进 / (b) 推后到第二轮 round 跟 XLeRobot 三轮全向实施一
起；reviewer 倾向未定）。

---

## 4. 风险与不确定项（候选 2 第一轮 reviewer round 必须拍板）

> 每条标 `Q<n>` 跟 v0_4_scripted_backend.md Spec round 1 同款编号风
> 格；第一轮 round 拍板项 = Q1-Q10；第二轮 round 拍板项 = Q11-Q20
> （已显式标注）。

### 4.1 第一轮 round 拍板（trait 草案 API）

#### Q1 · `ArmIkSolver` trait API 选型

候选 A / B / C（详 §3.1）；reviewer 倾向 A（最小 API + 后续扩展走
新 reviewer round 模式）。

#### Q2 · `IkError` 错误模型 variant 集

候选 α / β（详 §3.2）；reviewer 倾向 α + `InvalidInput { reason:
String }`。

#### Q3 · `Pose` / `Twist` / `Jacobian` 类型选型

候选 i / ii / iii（详 §3.3）；reviewer 倾向 i（自带类型 + 零依赖）。

#### Q4 · 多臂支持选型

候选 X / Y（详 §3.4）；reviewer 倾向 X（多 solver instance）。

#### Q5 · `BaseKinematics` 进第一轮 trait 草案？

**两选项**：
- (a) **进**第一轮：trait crate API 完整性 + §4.1 base.rs 已预写；
  XLeRobot 三轮全向跟双臂 IK 同步交付
- (b) **不进**第一轮（推后到第二轮 round 跟 XLeRobot 三轮全向实施一
  起）：第一轮专注 `ArmIkSolver` + 共享类型，trait crate first
  release 范围更小、reviewer 注意力更集中

reviewer 倾向：未定；本 Q **必须**第一轮 round 显式拍板。Spec 阶段
不得预设为 (a)（否则跟 §0 TL;DR 第一轮硬范围"仅 `ArmIkSolver`" 冲突）
也不得预设为 (b)。

#### Q6 · trait dyn-safety 约束

`ArmIkSolver` 必须 `dyn`-safe：方法不能 return `Self`，不能用
generic param（除非 `Self: Sized`），所有 trait bound 必须 `?Sized`
兼容。`BaseKinematics` 若 Q5 拍板进入第一轮，**同样**必须
`dyn`-safe（Q5 拍 (b) 推后则本约束随之挪到第二轮 round）。

**候选 2 第一轮 round 验收**：写一个 trait dyn-safety 锁测
（`fn assert_dyn_safe<T: ?Sized + ArmIkSolver>() {}` 编译期断言）；
Q5(a) 时同步加 `BaseKinematics` 同款锁测。

#### Q7 · `#![no_std]` 选项

`robot-kinematics` 是否 `#![no_std]`？

- 优：嵌入式场景可用；POD 类型天然 `no_std` 兼容；§4.1 "零 runtime
  依赖" 倾向支持
- 劣：`Jacobian` 用 `Vec<f64>` 需要 `alloc::vec::Vec`；`IkError`
  含 `String` 需要 `alloc::string::String`；如果选 `#![no_std,
  extern crate alloc]` 可用，但 `Box<dyn Error>` 等场景仍受限
- 倾向：**否**（保留 `std`）—— v0.4 主线运行环境 = Linux/macOS dev
  box + Orin AGX；`no_std` 是 over-engineering；如未来嵌入式需求出现
  再走单独 reviewer round 加 feature gate `default-features = ["std"]`

#### Q8 · trait crate 单元测试范围

第一轮 round trait crate 测试覆盖：

- (a) dyn-safety 锁测（编译期 assert）
- (b) Pose / Twist / Jacobian POD 类型构造 + 字段访问
- (c) `IkError` variant 穷举（沿候选 1 `action_kind_three_v04_variants`
  同款 exhaustive match 锁测）
- (d) `KinematicInfo` 构造 + 字段访问；**`BaseKinematicInfo` 仅
  Q5(a) 拍板进入第一轮时** 才进本测试范围（Q5(b) 时随
  `BaseKinematics` 一起推后到第二轮 round）

倾向：(a) + (c) 是硬 PASS；(b) + (d) 是 nice-to-have（其中
`BaseKinematicInfo` 子项跟 Q5 拍板联动）。

#### Q9 · S1 端接入形态预案（**第二轮**伪码；第一轮**严禁**落地）

> ⚠️ **HARD BOUNDARY**：以下伪码仅作 trait 形态可行性自检，**不**
> 是第一轮交付物。第一轮 round 已在 §0 TL;DR + §5 第 4 条锁死"零
> `crates/robot-s1` 改动"；本节代码块**不得**被 Spec / Plan 继承为第
> 一轮 commit 范围。第二轮 round 才真改 `coordinator.rs` —— 届时具
> 体字段名 / 可见性 / feature gate 边界由第二轮 reviewer round 重
> 新拍板，跟本节伪码无强绑定。

```rust
// crates/robot-s1/src/coordinator.rs —— 仅作伪码，第二轮 round
// 实施前 reviewer 再拍板，不进任何第一轮 commit
pub struct Coordinator {
    // ... existing fields ...
    #[cfg(feature = "backend-ik")]
    pub(crate) arm_ik: Option<Box<dyn robot_kinematics::ArmIkSolver>>,
    #[cfg(feature = "backend-ik")]
    pub(crate) base_kin: Option<Box<dyn robot_kinematics::BaseKinematics>>,
}
```

本节存在的唯一目的：让第一轮 reviewer round 在拍 trait API 形态
时，有"未来 S1 接入能不能落地"的可见性参考。**不**承担接入路径权威
性。

#### Q10 · 跟 candidate 1 第二轮 / 第二轮 round 并行节奏

v0.4_entry §3.3 拍板 "trait 草案 round 可跟候选 1 实现期重叠（不
同 commit chain）"。实际：候选 1 已 land；候选 2 第一轮 round 是否
立即开 / 等候选 2 第二轮 round 跟谁串？

倾向：**立即开**（候选 1 完结锚定 `11ac6aa`）。

### 4.2 第二轮 round 拍板（具体实施前置）

以下 Q11-Q20 在第一轮 round trait 拍板**之后**才动；第一轮 round
**不**讨论。仅作 CodeMap 完整性登记。

#### Q11 · `IkBackend` 状态机选型

单 phase（IK 一次求解 + 多 tick 复用 hold_command）vs 双 phase
（Plan 阶段 IK 求解 + Execute 阶段从 anchor 到 IK solution 做
joint-space ramp）。

#### Q12 · `IkPayload` fbs body 字段集

`target_pose: Pose` + `frame: string`（或 enum）+ `tolerance: f64`
+ `seed: [f64]`（可选）+ `arm: string` / `ArmHandle`。

#### Q13 · `ActionKindProto::Ik = 4` + `SCHEMA_IK_PAYLOAD = 0x0000_040C` append

沿候选 1 同款 atomic 6-step 注册链；schema_id 续号 `0x040C`（候选
1 用了 `0x040B`）。

#### Q14 · `robot-kinematics-xlerobot` 移植策略

路径 A（dimos arm_ik 闭式硬编码移植）vs 路径 B（vendor URDF +
parse）。

#### Q15 · XLeRobot 关节名重映射

dimos 内部命名 (`shoulder_pan` etc.) ↔ URDF 命名 (`Rotation` /
`Pitch` etc.) ↔ robot-system `hardware.joints` 配置命名。

#### Q16 · IK timeout 默认值

50Hz tick budget = 20ms / cycle；IK 求解最坏耗时容忍多少 ms？

#### Q17 · `backend-ik` Cargo feature 进 default？

倾向：**不**进 default（沿候选 1 `backend-scripted` 不同的策略 ——
IK 是可选功能，默认 build 不强制 link `robot-kinematics`；用户显式
开 feature）。

#### Q18 · cross-process e2e smoke 覆盖

候选 2 第二轮 round 是否新 `ik_backend_smoke` binary（沿 candidate
3 / 1 同款 `<phase>-smoke` feature 模式）？

#### Q19 · 真硬件回归节奏

Orin 真机 IK 端到端验证推后到第二轮 round land 之后单独安排（沿
v0.3 B 真硬件回归节奏）。

#### Q20 · §6.6.4 errata 是否需要 amend？

候选 2 第二轮 round land 后是否需要往 `docs/CHANGELOG.md` 加
"§6.6.4 IK 实施跟 errata 一致" 登记？倾向：**否**（实施跟 errata
已经一致；§6.7 5 不变量 0 偏移；不需要 amend 设计文档）。

---

## 5. 不在范围（候选 2 第一轮 round 边界）

按 v0.4_entry §3.3 + §5 hard boundaries + 候选 1 summary §6 接班入
口：

- **`robot-kinematics-xlerobot` impl**：留第二轮 round；第一轮 round
  仅 trait 草案
- **`IkBackend` 实施**：留第二轮 round；第一轮 round 仅写接入路径
  草稿（注释）
- **`ActionKindProto::Ik = 4` / `SCHEMA_IK_PAYLOAD` / fbs append**：
  留第二轮 round；第一轮 round 仅讨论编号位置（`0x040C` 续号）+
  atomic land 节奏 (沿候选 1 (1/3) 6-step pattern)
- **`robot-s1` 实施改动**：留第二轮 round；第一轮 round **零** s1 改
  动
- **真硬件回归 / Orin 真机 IK 验证**：留第二轮 round land 之后
- **dimos arm_ik.py 完整移植**：留第二轮 round 实施期
- **cubic / spline IK / Cartesian impedance control / trajectory
  backend**：留 v0.5+
- **§6.7 5 不变量任一条降级 / 升级**：5 条全保（候选 2 不触）
- **B4 frozen wire schema 现有 message / field 编号**：仅 append
  `ActionKindProto::Ik = 4`（候选 2 第二轮 round；第一轮 round 不
  触）
- **`docs/00_design.md` §4.1 模块地图正文 / §6.6.4 errata 正文**：
  候选 2 不动；trait 形态与 §4.1 预写文件结构 (`arm.rs` /
  `base.rs` / `pose.rs` / `error.rs`) 一致；若差异 → 走
  `docs/CHANGELOG.md` 偏离登记

---

## 6. 跟其它 milestone handoff / spec 的链接

- [`docs/handoffs/v0.4_entry.md`](../../docs/handoffs/v0.4_entry.md)
  §2.3 候选 2 范围 + §3.3 两轮 reviewer round 节奏 + §5 6 hard
  boundaries
- [`docs/handoffs/v0.4_scripted_backend_summary.md`](../../docs/handoffs/v0.4_scripted_backend_summary.md)
  §6 候选 2 接班 checklist + §3 关键契约审计（source_task_id 单调
  契约 / cancel 两层分离 / B3 §3.7 / B4 frozen schema 全保）
- [`docs/handoffs/v0.4_pubsub_cleanup_summary.md`](../../docs/handoffs/v0.4_pubsub_cleanup_summary.md)
  §3 关键契约审计（AC-C1 reaper / shutdown step 4 顺序 / Box::leak
  residual disclosure 延续）
- [`docs/handoffs/v0.3_b2_outline.md`](../../docs/handoffs/v0.3_b2_outline.md)
  `ActionBackend` trait / `BackendRegistry` / `ActionContext` /
  `ActionTickOutput` 原始拍板表 + reviewer round 锁定的"不扩 enum
  variant" / "不解锁 trait 方法签名" 边界
- [`docs/handoffs/v0.3_b3_summary.md`](../../docs/handoffs/v0.3_b3_summary.md)
  `ActionContext.command_seq` 字段扩展 + B3 §3.7 source_task_id ==
  TickState.cmd_seq 单调契约（候选 2 IkBackend 沿用）
- [`team-swe/codemap/v0_4_scripted_backend.md`](v0_4_scripted_backend.md)
  CodeMap 完整模板（9 段 + 18 条风险）
- [`team-swe/specs/v0_4_scripted_backend.md`](../specs/v0_4_scripted_backend.md)
  Spec round 1+2 amend + Plan v1 + round 1+2+3 amend + Plan-Execute
  diff 模板（候选 2 第二轮 round 沿用）
- `docs/00_design.md` §4.1 模块地图（`arm.rs` / `base.rs` /
  `pose.rs` / `error.rs` 预写文件结构）+ §6.6.4 errata（IK 是可选
  backend，非 Phase B gate）+ §6.7 5 条不变量（候选 2 全保）

---

## 7. 后续：reviewer round 1 + Spec

CodeMap commit 后，reviewer 开候选 2 **第一轮 round** = trait 草案
拍板：
- Q1-Q10 拍板（详 §4.1）
- ACK 后开 `team-swe/specs/v0_4_robot_kinematics.md`（沿候选 1 Spec
  模板）
- Spec round 1+2 amend → Plan outline → Plan round amend → 实施
  （第二轮 round 才动 robot-s1 / robot-kinematics-xlerobot；本轮
  仅 trait crate）

候选 2 第一轮 round 交付 commit 节奏（**Spec / Plan 阶段拍板**）：
- (1/n) `crates/robot-kinematics/Cargo.toml` + `src/lib.rs` 含
  `ArmIkSolver` trait + 共享类型 + 错误枚举 + dyn-safety 锁测；
  **Q5(a) 时**同含 `BaseKinematics` trait + `BaseKinematicInfo`
  + 同款锁测
- 沿 B2 / B3 / 候选 1 同款节奏；具体段数 reviewer Plan round 拍板

第二轮 round（独立 reviewer round）：
- `robot-kinematics-xlerobot` impl + `IkBackend` + S1 wiring + e2e
  smoke + summary

---

## 8. CodeMap 自检 checklist

- [x] §1 robot-s1 当前挂点（trait / context / registry / feature
  / 设计文档 §4.1+§6.6.4 / §6.7 影响）全覆盖
- [x] §2 dimos-deploy 迁移源（arm_ik.py / units.py / URDF）摸清
- [x] §3 trait 草案 5 选型 (A/B/C × α/β × i/ii/iii × X/Y × 进/不进)
  全列
- [x] §4 风险 Q1-Q20（第一轮 / 第二轮 分段）
- [x] §5 不在范围 9 条 hard boundary
- [x] §6 跟其它 milestone & spec 链接
- [x] §7 后续 reviewer round 节奏
