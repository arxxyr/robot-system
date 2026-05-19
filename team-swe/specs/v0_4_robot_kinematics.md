# Spec · v0.4 候选 2 round 1 — `robot-kinematics` `ArmIkSolver` trait 草案

> v0.4 候选 2 (robot-kinematics + IK backend) 拆**两轮 reviewer round**
> （v0.4_entry §3.3 拍板）。本 Spec **只**覆盖**第一轮 round**：
> `crates/robot-kinematics/` trait crate first release（trait + 纯
> 类型，零 runtime 依赖）；**零** `crates/robot-s1/` 改动；**零**
> 具体机器人实现；**零**新 schema append。
>
> 第二轮 round（独立 Spec）才覆盖：`robot-kinematics-xlerobot` impl +
> `IkBackend` 第三个真 action backend + S1 wiring + cross-process
> e2e smoke + summary。
>
> 写于 2026-05-18，commit `557e354` (候选 2 CodeMap pre-research land)
> 之后。CodeMap 见
> [`team-swe/codemap/v0_4_robot_kinematics.md`](../codemap/v0_4_robot_kinematics.md)。

---

## What

为 v0.4 候选 2 第一轮 round 落地 `crates/robot-kinematics/`
（workspace 蓝图已预留；详 `Cargo.toml` workspace 注释 `# 未来
crate ... # robot-kinematics`）的最小可用 API：

1. **新 workspace member** `crates/robot-kinematics/`：trait crate
   first release；零 runtime 依赖（仅 `thiserror` workspace dep 作错
   误 derive）；保留 `std`（**不**走 `no_std`）
2. **`ArmIkSolver` trait**（笛卡尔 → 关节弧度）+ **5 共享类型**（`Pose`
   / `Twist` / `Jacobian` / `KinematicInfo` / `IkSolution`）+
   **`IkError` 5 variant 错误枚举**
3. **dyn-safety 编译期锁测**（`ArmIkSolver` 必须 dyn-safe；CodeMap §4.1
   Q6 拍板）+ **`IkError` exhaustive match 锁测**（沿候选 1
   `action_kind_three_v04_variants` 同款）

具体 trait API / 字段集 / `IkError` variant 细节见 §6 AC + §7
Reviewer Q1-Q10 拍板表。

---

## 影响范围

### 必改文件（round 1 land 必须）

| 模块 | 改动 |
|---|---|
| `Cargo.toml` (workspace 根) | `members` 数组 append `"crates/robot-kinematics"`（位置：按字母序插入 `robot-health` 之后） |
| `crates/robot-kinematics/Cargo.toml`（新） | `[package]` workspace 继承 + `[dependencies] thiserror = { workspace = true }`；**不**加 nalgebra / urchin / pinocchio / placo 等 IK 第三方依赖；**不**加 `robot-core` / `robot-msg` 等 robot-system 内部 dep |
| `crates/robot-kinematics/src/lib.rs`（新） | `ArmIkSolver` trait + 5 类型 (`Pose` / `Twist` / `Jacobian` / `KinematicInfo` / `IkSolution`) + `IkError` 5 variant + `JointLimit` POD + dyn-safety 锁测 + `IkError` exhaustive match 锁测 |

### 可选改动（reviewer Plan round 决定）

| 模块 | 改动 |
|---|---|
| `crates/robot-kinematics/README.md`（新；可选） | 仅 crate 用途 + first release 状态 + 跟 §4.1 `arm.rs` 预写文件结构映射；reviewer Plan round 决定是否进 round 1（沿候选 1 同款 README 可选）；本表项不进 §"必改文件" 必跑 AC |

### 严禁改动（hard boundary）

reviewer round 1 拍板"严格无 robot-s1 改动"（详 §0 头注 + §7 Q9）。
以下文件 / 路径在 round 1 **零**改动；任一改动 = 触发 reviewer round
中止 / 改 Plan 拆段：

- `crates/robot-s1/**`（含 `action.rs` / `coordinator.rs` /
  `action_backend/**` / `rr_server/**` / `Cargo.toml` 全部不动）
- `crates/robot-rpc/**`（含 `proto/robot_control.proto` /
  `task_handlers.rs` 全部不动；`ActionKindProto::Ik` append **不**进
  round 1）
- `crates/robot-msg/**`（含 `schemas/*.fbs` / `schema_id.rs` /
  `markers.rs` / `size_tier.rs` / `build.rs` 全部不动；新
  `IkPayload` fbs body / `SCHEMA_IK_PAYLOAD` const 等**不**进 round 1）
- `crates/robot-s0/**`
- `crates/robot-adapters/**`
- `crates/robot-tests/**`（含 `Cargo.toml` 不加 `<phase>-smoke`
  feature；cross-process e2e smoke 推到第二轮 round）
- `crates/robot-kinematics-xlerobot/`（**不**进 round 1 workspace
  members；具体实现推到第二轮 round）
- `crates/robot-core/**`（保 §4.2 零 runtime 依赖原则；
  `robot-kinematics` 跟 `robot-core` **平级**，不互相 dep）
- `configs/**`（无新 config TOML）
- `docs/00_design.md` 正文（§4.1 / §6.6.4 任一段语义偏离 → 走
  `docs/CHANGELOG.md` 偏离登记）
- `docs/handoffs/**` 现有文件（仅本 Spec / Plan / round 1 summary 新
  增；不动现有 handoff）
- B3 §3.7 `source_task_id` 单调契约（round 1 不触；trait crate 不接
  `command_seq` / `FrameHeader.seq`）
- B4 frozen wire schema（round 1 不触）
- §6.7 不变量任一条降级 / 升级（5 条全保）

---

## 风险等级

**低**（vs 候选 1 "中"）。理由：

- trait crate first release = 纯类型 + trait 定义；**零** runtime 代码
  路径；**零** wire schema 改动；**零** S1 coordinator 改动
- workspace members append + 新 crate 落地是 append-only；不动现有任
  一 crate；revert 路径 = `git revert` 单 commit
- dyn-safety 锁测 + `IkError` exhaustive match 锁测 = 编译期 fail-
  fast；若 trait 形态非 dyn-safe 或 `IkError` 漏 variant，CI build
  阶段直接 fail
- 不进 hot path（trait 第一轮无 impl，无运行时调用）

主要风险点（详 §7 Q + CodeMap §4）：

1. **trait 形态选错** 后第二轮 round 重新拍板成本高：reviewer round 1
   Q1 拍板 (A) 最小 API；第二轮 round 如发现需 `solve_delta` / `FK`
   / `Jacobian`，走 reviewer round 扩 trait（B2 reviewer 锁定的"不解
   锁 trait 方法签名"边界 trait crate **本身** Q1 不适用 —— 候选 2
   是 trait crate 自身首次 release，trait 还没 "锁定"）
2. **`IkError` 错误模型预钉 clamp 行为**：reviewer 审查重点 #3。本
   Spec AC-6 显式声明 trait 层**不**约束 solver "unreachable 时返
   `Err(OutOfReach)` 还是 `Ok(clamped)` + informational metric"；
   XLeRobot 实施期（第二轮 round）按 dimos 行为或重新拍板（详 §6
   AC-6 + §8 不在范围）
3. **`Jacobian` Vec 字段在 hot path**：trait 第一轮无 impl，hot path
   alloc 风险推到第二轮 round；第一轮 `Jacobian` 仅类型定义不被调用
4. **`#![no_std]`** 推到未来：Q7 拍板 (b) **保留 `std`**；如未来嵌入
   式需求出现走单独 reviewer round 加 feature gate

---

## 回滚策略

round 1 是**纯 append-only 改动**（新 crate + workspace members
append；现有 crate 全 0 改动）；回滚 = revert 实施 commit。

各模块独立 revertable：

- workspace `Cargo.toml` `members` append revert → `cargo build
  --workspace` 不感知 `robot-kinematics` 存在 → 现有 12 crate 全
  绿（沿候选 1 同款 append-only 模式）
- `crates/robot-kinematics/` 整 crate revert（删目录 + workspace
  members append revert）= zero downstream 影响（不进 default
  build path；不被任何 crate dep）
- 任一 lib unit flaky → 标 `#[ignore]` + 留 follow-up issue，不回
  滚整 commit（沿候选 3 / 1 同款）

---

## 目标分支 / 交付形式

- 分支：`master`（直接 commit，沿候选 3 / 1 模式 + 每段 reviewer ACK）
- 交付：沿候选 1 / B3 / 候选 3 同款 3 段节奏倾向；具体段数 reviewer
  Plan round 拍板（**Spec 阶段不预钉死**；粗估 ~1-2 commit）
  * 候选拆分参考（仅参考；reviewer Plan round 拍板）：
    - 路径 A：单 commit (1/1) workspace members append + crate full
      land + 全 lib unit
    - 路径 B：拆 2 commit (1/2) 共享类型 + IkError + (2/2)
      ArmIkSolver trait + dyn-safety 锁测
- §6.7 不变量影响：5 条全保（**0** 降级 / **0** 升级）
- §1.5 状态机 / B3 wire 契约 / B4 frozen schema：round 1 全 0 触

---

## Acceptance Criteria

> AC 在代码之前定义。沿候选 1 同款；AC 数量 ~7（基础 AC + Quality
> Gate）；具体 reviewer round 1 拍板。

### AC-1: `crates/robot-kinematics/` workspace member append + Cargo.toml 零 runtime dep

- **前置条件**：round 1 commit 之后
- **验证**：
  * `Cargo.toml` workspace `members` 数组含 `"crates/robot-kinematics"`
  * `crates/robot-kinematics/Cargo.toml` `[dependencies]` 段**仅**含
    `thiserror = { workspace = true }`；**不**含 nalgebra / urchin /
    pinocchio / placo / kinpy / urdfpy 等任一 IK / 矩阵 / URDF 第三方
    依赖
  * `[dependencies]` 段**不**含 `robot-core` / `robot-msg` /
    `robot-s0` / `robot-s1` / `robot-rpc` / `robot-transport` /
    `robot-adapters` 等 robot-system 内部 crate dep（保 trait crate
    顶层独立性 + §4.1 "零 runtime 依赖" 设计原则）
  * `cargo build -p robot-kinematics --release` 通过
  * `cargo tree -p robot-kinematics --edges normal --no-dev` 直接
    依赖仅 `thiserror`（proc-macro 间接 dep 不算；`--edges normal`
    排除 build / dev edge）
- **验证方式**：`cargo build -p robot-kinematics --release` + `cargo
  tree -p robot-kinematics --edges normal --no-dev`

### AC-2: `ArmIkSolver` trait dyn-safe + 编译期锁测

- **前置条件**：round 1 commit 之后
- **验证**：
  * `pub trait ArmIkSolver: Send + Sync` 定义；方法签名**不**含 generic
    param（除非 `where Self: Sized` bound）/ **不**返 `Self` / 所有
    trait bound `?Sized` 兼容（Q6 dyn-safety 约束 ACK）
  * 编译期锁测 `_assert_arm_ik_solver_dyn_safe` 在 lib unit 内（沿候选
    1 lib unit 同款 commit 同 chain）；触发：在 lib unit 模块顶部声明
    `fn _assert_dyn_safe<T: ?Sized + ArmIkSolver>() {}` + 在 `#[test]
    fn arm_ik_solver_is_dyn_safe()` 调用 `_assert_dyn_safe::<dyn
    ArmIkSolver>()` 强制 monomorphize；若 trait 非 dyn-safe，本行编译
    fail（Q6 ACK）
- **验证方式**：`cargo test -p robot-kinematics --lib`

### AC-3: 共享类型 `Pose` / `Twist` / `Jacobian` / `KinematicInfo` / `IkSolution` 零 `nalgebra`

- **前置条件**：round 1 commit 之后
- **验证**：
  * `Pose { translation: [f64; 3], orientation: [f64; 4] }`（quaternion
    `[x, y, z, w]`，normalize 调用方保证；trait 层**不**校验）
  * `Twist { linear: [f64; 3], angular: [f64; 3] }`（m/s + rad/s）
  * `Jacobian { rows: usize, cols: usize, data: Vec<f64> }`（row-
    major；`rows == 6` for spatial Jacobian；trait 第一轮**不**强制
    invariant，只是定义）
  * `KinematicInfo { joint_count: usize, joint_names: Vec<String>,
    joint_limits: Vec<JointLimit> }` + `JointLimit { min_rad: f64,
    max_rad: f64 }`
  * `IkSolution { joints_rad: Vec<f64> }`（**最小**字段；**不**含
    `iterations` / `residual_error` / `clamped: bool` 等 metadata —
    避免预钉 solver "是否报告 clamp 行为" 的语义；详 AC-6 + §8 不
    在范围）
  * 全 5 类型 `#[derive(Debug, Clone)]`（+ POD `Pose` / `Twist` /
    `JointLimit` 加 `Copy`）；**不**用 `nalgebra::Isometry3` /
    `Vector3` / `Matrix6xX` 等 nalgebra 类型（保 Q3 拍板 (i) 零
    runtime 依赖）
- **验证方式**：`cargo test -p robot-kinematics --lib` + `cargo tree
  -p robot-kinematics --edges normal --no-dev` 不含 `nalgebra`

### AC-4: `IkError` 5 variant + `thiserror::Error` derive + exhaustive match 锁测

- **前置条件**：round 1 commit 之后
- **验证**：
  * `pub enum IkError` 5 variant（Q2 拍板 α + InvalidInput）：
    1. `InvalidInput { reason: String }`（运行时输入校验：NaN
       translation / quat 非规范化 / seed.len() != joint_count 等）
    2. `NoSolution`（数值法收敛失败；闭式解理论无穷尽边界）
    3. `OutOfReach { distance_m: f64 }`（目标 pose 离工作空间最远点
       的距离）
    4. `SingularityTooClose { condition_number: f64 }`（雅可比病态）
    5. `DeadlineExceeded`（数值法 deadline_ns 超过）
  * `#[derive(Debug, thiserror::Error)]` + `#[error("...")]` doc
    string per variant
  * `Send + Sync + 'static` auto-impl 通过（**不**含 `Box<dyn Error>`
    字段 —— 候选 β 反例；CodeMap §3.2 reviewer 倾向 α + InvalidInput
    ACK）
  * exhaustive match 锁测 `ik_error_variant_exhaustive`（沿候选 1
    `action_kind_three_v04_variants` 同款模式）：
    ```rust
    fn _exhaustive(e: IkError) -> &'static str {
        match e {
            IkError::InvalidInput { .. } => "InvalidInput",
            IkError::NoSolution => "NoSolution",
            IkError::OutOfReach { .. } => "OutOfReach",
            IkError::SingularityTooClose { .. } => "SingularityTooClose",
            IkError::DeadlineExceeded => "DeadlineExceeded",
        }
    }
    ```
    + `#[test] fn` 调用 + 5 variant 构造 + assert 字符串映射；若未
    来扩 variant 漏处理 compile fail
- **验证方式**：`cargo test -p robot-kinematics --lib`

### AC-5: `ArmIkSolver::solve` 签名 + `info()` 接口

- **前置条件**：round 1 commit 之后
- **验证**：
  * trait 方法签名（Q1 拍板 (A) 最小 API）：
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
  * `info()` 返 `&KinematicInfo`（trait 层不约束 lazy / eager 内部
    布局；solver 实现自决）
  * `solve()` 参数语义：
    - `target: &Pose`：目标 pose（trait 层**不**钉死 frame 抽象；
      reviewer round 1 Q1 ACK"最小 API"= 不引入 `FrameName` enum /
      `TransformTree` 等；frame 语义由 solver 实现 + 配置文档约定，
      第二轮 round 跟 `IkPayload` fbs body 一起拍板）
    - `seed: Option<&[f64]>`：可选 seed（数值法用；闭式解 ignore）
      —— `None` = 无 seed；`Some(slice)` 长度由 solver 实现按
      `info().joint_count` 校验，违反返 `IkError::InvalidInput {
      reason }`
    - `deadline_ns: Option<u64>`：可选 deadline（绝对 ns from
      `CLOCK_MONOTONIC_RAW`；语义跟 `robot-core::time::MonoRawNs(u64)`
      + `Deadline(MonoRawNs)` + `ActionRequest.deadline_ns: u64`
      仓库统一时间模型对齐；trait crate **不**依赖 robot-core，仅
      值语义对齐避免 signed conversion 损失精度 + 跨 crate 转换无意
      义 cast）—— `None` = 无 deadline；`Some(ns)` = 绝对超时 ns；
      solver 内部自查 + 返 `IkError::DeadlineExceeded`
- **验证方式**：`cargo test -p robot-kinematics --lib` + 编译期 trait
  签名锁测

### AC-6: trait 层不预钉 XLeRobot clamp 行为

- **前置条件**：AC-2 + AC-3 + AC-4 + AC-5 land 之后
- **验证**：
  * `IkSolution` **不**含 `clamped: bool` / `iterations: Option<u32>`
    / `residual_error: Option<f64>` 等 metadata 字段 —— 避免诱导
    solver 都强制报告内部行为
  * `IkError::OutOfReach` 在 trait 层是**可选**的 —— solver **可以**
    返 `Ok(IkSolution { joints_rad: clamped_to_workspace_boundary })`
    + 配套外部 informational metric（如 `tracing::warn!`）；**也可
    以**返 `Err(IkError::OutOfReach { distance_m })`；trait crate
    **不**强制语义
  * 本 AC 在 Spec / lib doc / Cargo.toml `[package].description` 三处
    显式声明（沿候选 1 hard boundary 同款双写防漂移）；reviewer 复核
    重点（详 §7 reviewer 审查重点 #3）
  * XLeRobot 实施期（第二轮 round）按 dimos arm_ik.py 现有 fail-safe
    clamp 行为或重新拍板（见 §8 不在范围 + 第二轮 round Q11 / Q14）
- **验证方式**：Spec round 1 + Plan-Execute diff 审计 + 实施 commit
  `cargo doc -p robot-kinematics --no-deps` 输出含 doc 段

### AC-7: 零 `crates/robot-s1/` 改动 + 零 wire schema 改动（reviewer 审查重点 #4）

- **前置条件**：round 1 commit 之后
- **验证**：
  * `git diff <round-1-base>..HEAD --name-only` 输出**不**含
    `crates/robot-s1/` / `crates/robot-rpc/` / `crates/robot-msg/`
    / `crates/robot-s0/` / `crates/robot-adapters/` /
    `crates/robot-tests/` 任一路径
  * `git diff <round-1-base>..HEAD --name-only` 输出**仅**含：
    - `Cargo.toml`（workspace members append 1 行）
    - `Cargo.lock`（cargo update 自动加 thiserror 间接）
    - `crates/robot-kinematics/Cargo.toml`（新）
    - `crates/robot-kinematics/src/lib.rs`（新）
    - 可选：`crates/robot-kinematics/README.md`（新）
  * `cargo build --workspace --release` 通过（验证既有 12 crate 不
    受 robot-kinematics 影响）
  * `cargo test --workspace --tests --features "robot-rpc/client" --
    --test-threads=1` 既有测试**0**回归
- **验证方式**：`git diff` 文件清单审计 + workspace build/test 不退
  化锁测

### AC-Q: Quality Gate（不进 AC，但是 PASS 条件）

- `cargo fmt --all` clean
- `cargo clippy --workspace --all-targets -- -D warnings` 双路 clean
  （含 / 不含 `robot-rpc/client` feature）
- `cargo test --workspace --tests --features "robot-rpc/client" --
  --test-threads=1` 既有 940+ PASS（候选 1 land 后基线）+ robot-
  kinematics 新加 ~3-5 lib unit 全过
- `cargo test -p robot-kinematics --lib` 全过
- `crash_restart_smoke --iterations 5`（real-s1 路径）**5/5 PASS**
  是**硬 gate**（沿候选 1 + 3 同款回归门槛）
- `crash_restart_smoke --use-mock-s1 --iterations 5`（mock-s1 路径）
  按**既有 baseline regression 记录**为 informational only（候选 1
  (1/3) 已确认在 `cac4515` Phase B 完结锚点上同款 FAIL，跟本 round 1
  改动无关；本 round 1 **不**写成硬 gate，沿候选 1 同款 follow-up
  策略不阻塞）
- `pubsub_cleanup_smoke --phase all`（候选 3 e2e）**不**退化：本轮
  改动**不**应触发候选 3 任一条件性 AC（AC-C1/C2/C3 全保 not
  triggered）
- `scripted_backend_smoke --phase all`（候选 1 e2e）**不**退化：本
  轮改动**不**应触发候选 1 AC-4/5/6/7 回归
- `cargo deny check` 过（thiserror workspace dep 已在白名单）
- `cargo run -p robot-msg --bin schema_lint` 过（不动 schema_id /
  fbs；既有锁测沿用）
- §6.7 5 条不变量真宣称保持（**0** 降级 / **0** 升级 / **0** 偏移）

---

## Reviewer round 1 拍板表 (Q1-Q10 已 ACK)

reviewer 在 CodeMap commit `557e354` 之后 round 1 拍板 Q1-Q10 默认
方向（"保守最小面"）：

| Q | reviewer round 1 ACK | 落 AC / 影响范围 |
|---|---|---|
| Q1 trait API 选型 | **(A) 最小 ArmIkSolver**（`info()` + `solve()` 两方法；`solve_delta` / FK / Jacobian 推到未来 reviewer round 扩 trait） | AC-5 |
| Q2 IkError 错误模型 | **α + InvalidInput**（§4.1 4 类 + `InvalidInput { reason: String }` 第 5 个；**不**走候选 β `Box<dyn Error>`） | AC-4 |
| Q3 Pose / Twist / Jacobian 类型选型 | **(i) 自带 POD 类型，零 nalgebra 依赖**（保 §4.1 "零 runtime 依赖" 原则；XLeRobot 闭式解不需要矩阵运算；如未来数值法需要，在 `robot-kinematics-<robot>` 实现内部 link nalgebra 不污染 trait crate） | AC-3 + AC-1 |
| Q4 多臂支持选型 | **(X) 多 solver instance**（per-arm）（trait 不暴露 side 字段；XLeRobot 双臂 = S1 持两个 `Box<dyn ArmIkSolver>` instance；跟 dimos `ArmIK(side)` 模式一致） | AC-5（trait 不持 side） |
| Q5 BaseKinematics 进第一轮？ | **(b) 推到第二轮 round**（第一轮**仅**钉 `ArmIkSolver` trait + 共享类型；`BaseKinematics` trait + `BaseKinematicInfo` + `body_twist_to_wheel_rad_s` / `wheel_rad_s_to_body_twist` 等推到第二轮 round 跟 XLeRobot 三轮全向实施一起；CodeMap §3.5 草案保留作第二轮 round 输入参考） | §8 不在范围 + AC-1（lib.rs 不含 BaseKinematics） |
| Q6 trait dyn-safety 约束 | **必须 dyn-safe + 编译期锁测**（`fn _assert_dyn_safe<T: ?Sized + ArmIkSolver>() {}` + `#[test]` 强制 monomorphize via `_assert_dyn_safe::<dyn ArmIkSolver>()`；若 trait 非 dyn-safe 编译期 fail） | AC-2 |
| Q7 `#![no_std]` 选项 | **(b) 否，保留 `std`**（v0.4 主线运行环境 = Linux / macOS dev box + Orin AGX；`no_std` over-engineering；未来嵌入式需求出现走单独 reviewer round 加 `default-features = ["std"]` feature gate） | AC-1（不加 `#![no_std]`） |
| Q8 trait crate 单元测试范围 | **dyn-safety + IkError exhaustive match 硬 PASS**；POD 类型构造测试 nice-to-have（reviewer 可加进 round 1 commit；不阻塞 ACK） | AC-2 + AC-4 |
| Q9 S1 端接入形态预案 | **仅保留 CodeMap §3.5 / Q9 伪码**；**不**进 round 1 交付；第一轮**严格零** `crates/robot-s1` 改动 | AC-7 + §"严禁改动" hard boundary |
| Q10 跟候选 1 / 候选 2 第二轮 round 节奏 | **立即开 Spec**（候选 1 已 land `11ac6aa`；候选 2 CodeMap 已 land `557e354`；本 Spec round 1 进入实施前唯一前置 = reviewer round 2 ACK） | §10 后续 |

> Q1-Q10 ACK 后，Spec round 2 内不重新拍板任一 Q；偏离需走
> `team-swe/specs/v0_4_robot_kinematics.md` §P.4 Plan-Execute diff
> 模板（沿候选 1 同款 appendix-only 登记机制）。

---

## reviewer 审查重点 (round 1 实施期硬 gate)

reviewer 给 Q1-Q10 拍板时同时锁 4 条审查重点；本 Spec 把每条 design-
in 到具体 AC + 严禁改动列表：

| # | 审查重点 | 落地 |
|---|---|---|
| 1 | trait 是否真的 dyn-safe | AC-2 编译期 `_assert_dyn_safe::<dyn ArmIkSolver>()` 锁测 + Q6 ACK |
| 2 | 类型是否零 runtime 依赖 | AC-1 Cargo.toml 仅 `thiserror = { workspace = true }` + AC-3 全 5 类型用 `[f64; N]` / `Vec<f64>` / `usize` 等 std POD；`cargo tree` 不含 `nalgebra` 校验 + Q3(i) ACK |
| 3 | 错误模型是否不会把 XLeRobot clamp 行为提前钉死 | AC-6 `IkSolution` 最小字段（**不**含 `clamped: bool` 等 metadata）+ `IkError::OutOfReach` trait 层可选语义（solver 选 `Ok(clamp)` 或 `Err(OutOfReach)` 自决）+ Spec / lib doc / `[package].description` 三处显式声明 |
| 4 | 第一轮是否严格没有 robot-s1 改动 | AC-7 `git diff --name-only` 文件清单审计 + §"严禁改动" hard boundary 14 条路径 + `cargo build --workspace` 既有 12 crate 不退化锁测 |

---

## 不在范围（候选 2 round 1 边界）

按 v0.4_entry §3.3 + CodeMap §5 + 上面 §"严禁改动" + Q1-Q10 拍板：

- **`BaseKinematics` trait + `BaseKinematicInfo`** — Q5(b) ACK；推
  到第二轮 round；CodeMap §3.5 草案作第二轮 round 输入参考；本 round 1
  **不**创建 `base.rs` 文件（§4.1 预写文件结构跟实际差异走
  `docs/CHANGELOG.md` 偏离登记）
- **`robot-kinematics-xlerobot` impl** — 第二轮 round；本 round 1
  workspace `members` 数组**不**含 `crates/robot-kinematics-xlerobot`
- **`IkBackend` 第三个真 action backend impl** — 第二轮 round；本
  round 1 `crates/robot-s1/src/action_backend/` 不动
- **`ActionKindProto::Ik = 4` / `ActionKindFb::Ik = 4` /
  `ActionKind::Ik` enum append** — 第二轮 round；本 round 1
  `proto/robot_control.proto` / `action_request.fbs` /
  `action.rs::ActionKind` 全不动
- **`IkPayload` fbs body + `SCHEMA_IK_PAYLOAD = 0x0000_040C` 注册链**
  — 第二轮 round；本 round 1 schema 注册链 6 步**全 0** 触
- **frame 抽象 / `FrameName` enum / `TransformTree`** — 第二轮
  round 跟 `IkPayload` fbs body 字段集一起拍板（Q12）；本 round 1
  trait `solve(&self, target: &Pose, ...)` 不引入 frame 参数
- **`solve_delta(current_angles)` / FK / Jacobian** — 未来扩 trait
  reviewer round；本 round 1 trait 仅 `info()` + `solve()` 两方法
  （Q1(A) 最小 API ACK）
- **`nalgebra` optional feature** — 未来如需要走单独 reviewer round
  + feature gate；本 round 1 trait crate 零 nalgebra
- **XLeRobot clamp 行为 / fail-safe 策略选型** — 第二轮 round 跟
  `robot-kinematics-xlerobot` impl 一起拍板（Q14）；本 round 1 trait
  层**不**预钉
- **关节名重映射**（dimos `shoulder_pan` etc. ↔ URDF `Rotation`
  etc.）— 第二轮 round（Q15）；trait 层 `KinematicInfo::joint_names`
  字段定义但调用方填什么由 solver 实现自决
- **URDF 加载** — 第二轮 round 可选（路径 A 闭式硬编码 vs 路径 B
  vendor URDF + parse；Q14）；本 round 1 trait crate 零 URDF 依赖
- **`IkBackend` cross-process e2e smoke** — 第二轮 round（`ik_
  backend_smoke` feature-gated binary，沿候选 1 / 3 同款 `<phase>-
  smoke` 模式；Q18）
- **真硬件回归 / Orin 真机 IK 端到端** — 第二轮 round land 之后单
  独安排（沿 v0.3 B 真硬件回归节奏；Q19）
- **§6.7 5 不变量任一条降级 / 升级** — 5 条全保（round 1 不触）
- **`docs/00_design.md` §4.1 / §6.6.4 正文** — 不动；trait 形态跟
  §4.1 预写文件结构 (`arm.rs` 含 `ArmIkSolver` / `KinematicInfo` /
  `IkSolution` / `IkError`) 一致；`base.rs` 第一轮**不**创建是
  Q5(b) ACK 的必然结果，跟 §4.1 文件结构差异属"预写超前于实施"，**不**
  走 `docs/CHANGELOG.md` 偏离登记（设计文档预写本就是 placeholder；
  无语义违反）
- **`docs/handoffs/` 现有 handoff 文件** — 仅本 Spec + round 1
  summary 新加；不动现有 handoff

---

## 跟其它 milestone handoff / spec 的链接

- [`docs/handoffs/v0.4_entry.md`](../../docs/handoffs/v0.4_entry.md)
  §2.3 候选 2 范围 + §3.3 两轮 reviewer round 节奏 (ACK)
- [`docs/handoffs/v0.4_scripted_backend_summary.md`](../../docs/handoffs/v0.4_scripted_backend_summary.md)
  §3 关键契约审计（source_task_id 单调 / cancel 两层分离 / 两道
  byte gate / ActionRpcStatus 映射；候选 2 第二轮 round IkBackend
  沿用）+ §6 候选 2 接班 checklist + §7 Plan-Execute diff 3 项
  （沿用模板）
- [`docs/handoffs/v0.4_pubsub_cleanup_summary.md`](../../docs/handoffs/v0.4_pubsub_cleanup_summary.md)
  §3 关键契约审计（AC-C1 reaper / shutdown step 4 顺序 / Box::leak
  residual disclosure；候选 2 不触但回归门槛沿用）
- [`team-swe/codemap/v0_4_robot_kinematics.md`](../codemap/v0_4_robot_kinematics.md)
  完整 Pre-Research CodeMap（8 段 + Q1-Q20 待拍板表；Q1-Q10 本 Spec
  round 1 ACK，Q11-Q20 推到第二轮 round）
- [`team-swe/specs/v0_4_scripted_backend.md`](v0_4_scripted_backend.md)
  Spec round 1+2 amend 模板 + Plan v1 + round 1+2+3 amend + Plan-
  Execute diff §P.4 模板（候选 2 round 1 Plan / 第二轮 round Spec
  全沿用）
- [`docs/handoffs/v0.3_b2_outline.md`](../../docs/handoffs/v0.3_b2_outline.md)
  `ActionBackend` trait / `BackendRegistry` / `ActionContext` /
  `ActionTickOutput` 原始拍板表 + reviewer round 锁定的"不扩 enum
  variant" / "不解锁 trait 方法签名" 边界（本 trait crate `ArmIkSolver`
  是新 crate 自身 first release；B2 trait 锁定边界不直接适用，但模
  式抄）
- [`docs/handoffs/v0.3_b3_summary.md`](../../docs/handoffs/v0.3_b3_summary.md)
  `ActionContext.command_seq` 字段扩展 + B3 §3.7 source_task_id 单
  调契约（候选 2 第二轮 round IkBackend 实施时直接复用；本 round 1
  trait crate 不触）
- `docs/00_design.md` §4.1 模块地图（`arm.rs` 预写文件结构 +
  `ArmIkSolver` / `KinematicInfo` / `IkSolution` / `IkError` 预写
  类型名）+ §6.6.4 errata（IK 是可选 backend；本 round 1 trait crate
  落地不改变这条 errata 语义；trait crate 单纯 first release，**不**
  把 IK 升级为 S1 / Phase B gate）+ §6.7 5 条不变量（本轮全保）

---

## 后续：reviewer round 2 + Plan

Spec round 1 commit 后，reviewer 进 **Spec round 2** 复核：
- AC-1..AC-7 + AC-Q 边界审 / Q1-Q10 拍板表是否准确落 AC / 严禁改动
  14 路径列表是否覆盖完整 / 审查重点 4 条 design-in 是否双写
- 如有阻断 → Spec round 2 amend commit；零阻断 → ACK 进 Plan 阶段

**Plan 阶段**（沿候选 1 / 3 同款 Plan v1 + round amend 模式）：
- Plan outline draft v1 commit：commit 节奏拆分 (1/1 单 commit vs
  1/2 + 2/2 拆 2 commit) + P.0..P.4 段
- reviewer Plan round 1+2+... amend → Plan Approved → 进入实施
- 实施期偏离 Plan 任何决策 → 登记 §P.4 Plan-Execute diff（沿候选 1
  同款 appendix-only）

**实施期 commit 节奏**（粗估，Plan 阶段拍板）：

```
✨ feat: v0.4 候选 2 round 1 (1/1) robot-kinematics ArmIkSolver
        trait crate first release land（workspace member append +
        ArmIkSolver trait + 5 共享类型 + IkError 5 variant +
        dyn-safety 锁测 + IkError exhaustive match 锁测；零
        robot-s1 改动；零 wire schema 改动）
```

或拆 2 commit：

```
✨ feat: v0.4 候选 2 round 1 (1/2) robot-kinematics 共享类型 +
        IkError 5 variant land
✨ feat: v0.4 候选 2 round 1 (2/2) robot-kinematics ArmIkSolver
        trait + dyn-safety 锁测 land
```

第二轮 round 入口（**独立 reviewer round + 独立 Spec**）：
- 范围：`robot-kinematics-xlerobot` impl + `IkBackend` 第三个真
  action backend + S1 持 `Option<Box<dyn ArmIkSolver>>` + cross-
  process e2e smoke (`ik_backend_smoke` feature-gated binary) +
  summary
- 节奏：沿 B2 / B3 / 候选 1 同款 outline → reviewer round 1+2+... →
  (1/n) → ... → (n/n) → summary
- 触发：本 round 1 land + summary 归档 + reviewer 第二轮 round entry
  ACK 之后
