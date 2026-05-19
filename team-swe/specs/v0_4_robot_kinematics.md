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
  * `crates/robot-kinematics/Cargo.toml` 含 `[lints]` 段 + `workspace
    = true`（继承 workspace lint 基线；既有 workspace member **全部
    显式继承**；漏写 = 绕开 workspace lint 基线 → 阻断）
  * `cargo build -p robot-kinematics --release` 通过
  * `cargo tree -p robot-kinematics --edges normal,no-proc-macro`
    直接依赖仅 `thiserror`（`--edges normal,no-proc-macro` 排除 build
    / dev edge + `thiserror::Error` derive 间接拉的 `proc-macro2` /
    `syn` / `quote` 等 proc-macro crate；`--no-dev` 不是独立 flag，
    `no-dev` / `no-proc-macro` 都是 `--edges` 的 value）
- **验证方式**：`cargo build -p robot-kinematics --release` + `cargo
  tree -p robot-kinematics --edges normal,no-proc-macro`（直接 dep
  集合 == `{thiserror}`）

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
  -p robot-kinematics --edges normal,no-proc-macro` 不含 `nalgebra`

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
    - `Cargo.toml`（workspace `members` 数组 append `"crates/robot-
      kinematics"` 1 行 **+** "未来 crate" 注释段 `robot-kinematics`
      注释行删除；两改动同 commit；详 §P.2.1.a）
    - `Cargo.lock`（新增 `[[package]] name = "robot-kinematics"`
      stanza + 配套 dependency / checksum 行；`thiserror` 已在 lock
      里跟 `robot-core` 等共享，本轮**不**新增 thiserror 间接行）
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
- 重型 smoke 回归门槛（`pubsub_cleanup_smoke` / `scripted_backend_smoke`）
  **具体模式由 §Plan §P.3 拍板项 #3 决定**：
  * **(a) full e2e**：双 `--phase all` 不退化（候选 3 AC-C1/C2/C3 全保
    not triggered + 候选 1 AC-4/5/6/7 不回归）
  * **(b) compile gate + selftest only**（plan author 倾向）：跑 §P.3.3a
    5 条命令（双 binary compile + 双 selftest + 双 `--bin` cargo test）；
    判据沿 §P.3.3a "通过判据" 段；本 round 1 零 runtime 代码路径 + 零
    wire schema 改动 → compile + selftest 已足够覆盖候选 1 / 3 不退化
  * **(c) 完全不跑**：reviewer Plan round 拍板 (c) 时启用；本轮 plan
    author **不**倾向（compile gate 兜底成本极低）
  * AC-Q 本身**不**预钉 (a)/(b)/(c)；Plan round 1 ACK 后两文锁同一选项
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

---

## Plan

> Plan outline draft v1（reviewer ACK pending）。基于 Spec §AC + Q1-Q10
> 拍板表 + §"严禁改动" hard boundary + reviewer 审查重点 4 条硬输入；
> **不**重新决策 Q1-Q10 / AC 边界 / 14 路径 hard boundary。Spec round
> 1 commit `64cf312` push 后开 Plan round 1。

### P.0 工作基线

- 起点：`master` HEAD = `64cf312`（Spec round 1 land）。
- Q 拍板硬输入：Q1(A) 最小 `ArmIkSolver` / Q2(α + InvalidInput) IkError
  5 variant / Q3(i) 自带 POD 零 nalgebra / Q4(X) 多 solver instance /
  Q5(b) BaseKinematics 推到第二轮 round / Q6 dyn-safe + 编译期锁测 /
  Q7(b) 保留 std / Q8 dyn-safety + IkError exhaustive 硬 PASS / Q9 仅
  CodeMap 伪码、零 robot-s1 改动 / Q10 立即开 Spec（详 §"Reviewer
  round 1 拍板表"）。
- 审查重点硬输入：(1) trait dyn-safe / (2) 零 runtime 依赖 / (3) 不预
  钉 XLeRobot clamp / (4) 零 robot-s1 改动（详 §"reviewer 审查重点"）。
- 不开新分支（沿候选 1 / 3 + B4 模式直接 commit 到 `master` + 每段
  reviewer ACK）。
- **第二轮 round 范围**（`robot-kinematics-xlerobot` impl + `IkBackend`
  + S1 wiring + e2e smoke）**不**进本 Plan；独立 reviewer round +
  独立 Spec 触发（沿 Spec §"后续：reviewer round 2 + Plan"）。
- 全 Plan 严格基于 Spec §AC + §Reviewer 拍板；**不**重新决策任何
  Q1-Q10 / AC 边界 / 14 路径 hard boundary。

### P.1 commit 节奏总览

> 范围极小 + append-only + 现有 12 crate 0 改动 → **plan author 倾向
> 单 commit (1/1)**。具体节奏 reviewer Plan round 1 拍板（详 §P.3
> 拍板项 #1）。

**Happy path A — 单 commit (1/1)**（plan author 倾向；AC-1 至 AC-7 全
过 + Quality Gate 全过 + 范围极小不需拆段）：

```
✨ feat: v0.4 候选 2 round 1 (1/1) robot-kinematics ArmIkSolver
        trait crate first release land（workspace members append +
        crates/robot-kinematics Cargo.toml + lib.rs：5 共享类型 +
        IkError 5 variant + ArmIkSolver trait + dyn-safety 编译期
        锁测 + IkError exhaustive match 锁测 + 3-5 lib unit；零
        runtime 依赖（仅 thiserror）；零 robot-s1 / robot-rpc /
        robot-msg / robot-s0 / robot-adapters 改动；零 wire schema
        改动）
📝 docs: v0.4 候选 2 round 1 summary 归档
```

粗估 1 实施 commit + summary = 2 commit chain。

**Happy path B — 拆 2 commit (1/2) + (2/2)**（备选；仅 reviewer Plan
round 1 拍 (1/2) 时启用）：

```
✨ feat: v0.4 候选 2 round 1 (1/2) robot-kinematics workspace
        member + 共享类型 + IkError land（workspace members append +
        crates/robot-kinematics Cargo.toml + lib.rs Pose / Twist /
        Jacobian / KinematicInfo / JointLimit / IkSolution + IkError
        5 variant + IkError exhaustive match 锁测 + 2 lib unit；
        ArmIkSolver trait 推到 (2/2)）
✨ feat: v0.4 候选 2 round 1 (2/2) robot-kinematics ArmIkSolver
        trait + dyn-safety 锁测 land（lib.rs append ArmIkSolver
        trait + dyn-safety 编译期锁测 + 1-3 lib unit）
📝 docs: v0.4 候选 2 round 1 summary 归档
```

粗估 2 实施 commit + summary = 3 commit chain。

**(1/2) → (2/2) 拆段触发**（详 §P.3 拍板项 #1）：
- reviewer 倾向"小段更易审"或"共享类型 vs trait 形态独立 review"
- 实施期 (1/2) 编译过但 (2/2) dyn-safety 锁测发现 trait 需调整 →
  Plan-Execute diff #3 登记 (详 §P.4)

**拒绝 3+ commit 拆段**：范围实在不足以拆 ≥ 3 段（共享类型 + trait +
锁测 = 3 个并列概念但代码量 ~80-120 行；拆 3 段 = 过度细化）；如
reviewer Plan round 拍 3+ 走 Plan-Execute diff #3。

### P.2 工作流（按阶段顺序）

#### P.2.1 [commit (1/1) OR (1/2)] workspace member + Cargo.toml + 共享类型 + IkError

##### P.2.1.a `Cargo.toml` workspace `members` append

`Cargo.toml`（workspace 根）：

```diff
 members = [
     "crates/robot-adapters/feetech",
     "crates/robot-adapters/mock",
     "crates/robot-cli",
     "crates/robot-core",
     "crates/robot-health",
+    "crates/robot-kinematics",
     "crates/robot-logging",
     "crates/robot-msg",
     "crates/robot-rpc",
     "crates/robot-s0",
     "crates/robot-s1",
     "crates/robot-tests",
     "crates/robot-transport",
 ]
```

同步删 `Cargo.toml:25` "未来 crate" 注释段里的 `robot-kinematics` 行
（首批落地不再算"未来"）；保留 `robot-kinematics-xlerobot` /
`robot-kinematics-piper` 注释行（第二轮 round / v0.6+ 才进）。

##### P.2.1.b `crates/robot-kinematics/Cargo.toml`（新）

沿 `crates/robot-core/Cargo.toml` 同款 workspace 继承模板：

```toml
[package]
name         = "robot-kinematics"
description  = "通用机器人运动学 trait + 纯类型；零 runtime 依赖。trait 层不预钉 XLeRobot clamp 行为（AC-6）。"
version    .workspace = true
edition    .workspace = true
license    .workspace = true
authors    .workspace = true
repository .workspace = true
readme     .workspace = true
publish    .workspace = true

[dependencies]
# IkError derive thiserror::Error；trait crate 仅此一项 runtime dep。
# 严禁加 nalgebra / urchin / pinocchio / placo / kinpy / urdfpy 等 IK /
# 矩阵 / URDF 第三方依赖（Q3(i) ACK，AC-1 锁）。
# 严禁加 robot-core / robot-msg / robot-s0 / robot-s1 / robot-rpc /
# robot-transport / robot-adapters 等 robot-system 内部 crate dep（保
# §4.1 trait crate 顶层独立性 + §4.2 零 runtime 依赖原则；trait crate
# 跟 robot-core 平级，不互相 dep）。
thiserror = { workspace = true }

[lints]
workspace = true
```

**必须落项**：
- `[lints] workspace = true` —— 继承 workspace lint 基线（既有
  workspace member **全部显式继承**：`robot-adapters/feetech` /
  `robot-adapters/mock` / `robot-cli` / `robot-core` / `robot-health`
  / `robot-logging` / `robot-msg` / `robot-rpc` / `robot-s0` /
  `robot-s1` / `robot-tests` / `robot-transport`）；漏写 = 绕开
  workspace lint 基线 → reviewer round 阻断（AC-1 已锁）

**严禁改动**：
- `[package].description` 的 "trait 层不预钉 XLeRobot clamp 行为
  （AC-6）" 锚句 — reviewer 审查重点 #3 三处双写之一（详 Spec §"reviewer
  审查重点"）；删除 / 改写 = Plan-Execute diff
- `[features]` 段 — round 1 **不**加任何 feature（`std` / `nalgebra` /
  `no_std` 等推到未来单独 reviewer round；Q7(b) ACK）

##### P.2.1.c `crates/robot-kinematics/src/lib.rs`（新）共享类型 + IkError

文件起首 doc 段（reviewer 审查重点 #3 第二处双写）：

```rust
//! 通用机器人运动学 trait + 纯类型。
//!
//! # 设计原则
//!
//! - **零 runtime 依赖**：仅 `thiserror` derive；不依赖 `nalgebra` /
//!   `robot-core` / `robot-msg` 等。
//! - **trait dyn-safe**：`ArmIkSolver` 可作 `Box<dyn ArmIkSolver>`
//!   存于 S1 coordinator（详 `ArmIkSolver` doc）。
//! - **不预钉 XLeRobot clamp 行为**：`IkSolution` 最小字段（不含
//!   `clamped: bool` / `iterations` / `residual_error` 等 metadata）；
//!   `IkError::OutOfReach` 在 trait 层**可选** —— solver 可返
//!   `Ok(IkSolution { joints_rad: clamped })` + 外部 informational
//!   metric，也可返 `Err(OutOfReach { distance_m })`，trait crate 不
//!   强制语义（XLeRobot 实施期第二轮 round 拍板，沿 dimos arm_ik.py
//!   或重新选型）。
//!
//! # 第一轮 round 不在范围
//!
//! 详 `team-swe/specs/v0_4_robot_kinematics.md` §"不在范围"：
//! - `BaseKinematics` trait + `BaseKinematicInfo`（推到第二轮 round）
//! - `robot-kinematics-xlerobot` 实现（第二轮 round）
//! - `IkBackend` 第三个真 action backend（第二轮 round）
//! - frame 抽象 / `FrameName` enum / `TransformTree`（第二轮 round）
```

**5 共享类型**（沿 Spec AC-3 字段集 + Q3(i) ACK）：

```rust
/// 笛卡尔 pose（translation + quaternion XYZW）。
///
/// quaternion 顺序 `[x, y, z, w]`；normalize 由调用方保证，trait 层
/// **不**校验。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pose {
    pub translation: [f64; 3],
    pub orientation: [f64; 4],
}

/// 笛卡尔 twist（linear m/s + angular rad/s）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Twist {
    pub linear: [f64; 3],
    pub angular: [f64; 3],
}

/// row-major spatial Jacobian；trait 第一轮**不**强制 `rows == 6` /
/// `data.len() == rows * cols` 等 invariant，只是类型定义。
#[derive(Debug, Clone)]
pub struct Jacobian {
    pub rows: usize,
    pub cols: usize,
    pub data: Vec<f64>,
}

/// 单关节 limit（弧度）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct JointLimit {
    pub min_rad: f64,
    pub max_rad: f64,
}

/// 运动学元数据（关节数 + 名 + limit）；solver 实现自决 lazy / eager。
#[derive(Debug, Clone)]
pub struct KinematicInfo {
    pub joint_count: usize,
    pub joint_names: Vec<String>,
    pub joint_limits: Vec<JointLimit>,
}

/// IK 解（仅 joints_rad；**不**含 metadata —— 避免预钉 solver "是否
/// 报告 clamp 行为" 的语义，详 AC-6 + 模块 doc）。
#[derive(Debug, Clone)]
pub struct IkSolution {
    pub joints_rad: Vec<f64>,
}
```

**`IkError` 5 variant**（沿 Spec AC-4 + Q2 α + InvalidInput ACK）：

```rust
#[derive(Debug, thiserror::Error)]
pub enum IkError {
    #[error("invalid IK input: {reason}")]
    InvalidInput { reason: String },

    #[error("IK numerical solver failed to converge")]
    NoSolution,

    #[error("target pose out of workspace reach (distance {distance_m:.4} m)")]
    OutOfReach { distance_m: f64 },

    #[error("Jacobian singularity too close (condition number {condition_number:.3e})")]
    SingularityTooClose { condition_number: f64 },

    #[error("IK solver deadline exceeded")]
    DeadlineExceeded,
}
```

##### P.2.1.d 共享类型 + `IkError` lib unit（AC-3 + AC-4）

最少 2 测（含 IkError exhaustive match 锁测）：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// IkError exhaustive match 锁测（沿候选 1 `action_kind_three_v04_
    /// variants` 同款；漏 variant 编译期 fail）。
    fn _exhaustive(e: IkError) -> &'static str {
        match e {
            IkError::InvalidInput { .. } => "InvalidInput",
            IkError::NoSolution => "NoSolution",
            IkError::OutOfReach { .. } => "OutOfReach",
            IkError::SingularityTooClose { .. } => "SingularityTooClose",
            IkError::DeadlineExceeded => "DeadlineExceeded",
        }
    }

    #[test]
    fn ik_error_variant_exhaustive() {
        assert_eq!(_exhaustive(IkError::InvalidInput { reason: "x".into() }), "InvalidInput");
        assert_eq!(_exhaustive(IkError::NoSolution), "NoSolution");
        assert_eq!(_exhaustive(IkError::OutOfReach { distance_m: 0.1 }), "OutOfReach");
        assert_eq!(
            _exhaustive(IkError::SingularityTooClose { condition_number: 1.0e6 }),
            "SingularityTooClose",
        );
        assert_eq!(_exhaustive(IkError::DeadlineExceeded), "DeadlineExceeded");
    }

    /// POD 类型构造测（reviewer Q8 ACK"nice-to-have"，本轮直接落避免
    /// (2/2) 拆段时本段空荡）。
    #[test]
    fn pose_twist_jacobian_pod_constructable() {
        let _p = Pose { translation: [0.0; 3], orientation: [0.0, 0.0, 0.0, 1.0] };
        let _t = Twist { linear: [0.0; 3], angular: [0.0; 3] };
        let _j = Jacobian { rows: 6, cols: 3, data: vec![0.0; 18] };
        let _info = KinematicInfo {
            joint_count: 1,
            joint_names: vec!["j0".into()],
            joint_limits: vec![JointLimit { min_rad: -1.0, max_rad: 1.0 }],
        };
    }
}
```

##### P.2.1.e Quality Gate（commit (1/1) 或 (1/2)）

- `cargo fmt --all --check` clean
- `cargo clippy --workspace --all-targets -- -D warnings` 双路 clean
  （含 / 不含 `robot-rpc/client` feature）
- `cargo build --workspace --release` 通过（验证既有 12 crate 不受
  `robot-kinematics` 影响）
- `cargo build -p robot-kinematics --release` 通过
- `cargo test -p robot-kinematics --lib`：本段新加测全过（拆 (1/2) 时
  ≥ 2 测；单 commit (1/1) 时跟 §P.2.2.c 合并 ≥ 3-5 测）
- `cargo test --workspace --tests --features "robot-rpc/client" --
  --test-threads=1`：既有 940+ PASS（候选 1 land 后基线）全保
- `cargo tree -p robot-kinematics --edges normal,no-proc-macro` 直接
  依赖**仅** `thiserror`；不含 `nalgebra` / `robot-core` / `robot-msg`
  等（`--no-dev` 不是独立 flag；`no-dev` / `no-proc-macro` 都是
  `--edges` 的 value）
- `cargo deny check`：thiserror workspace dep 已在白名单（既有 6
  crate 已 dep；本轮**不**新加 license / advisory exception）
- `cargo doc -p robot-kinematics --no-deps` 过：lib.rs 顶 `//!` doc
  段 + `ArmIkSolver` trait doc + `IkError` variant doc 全部 render
  无 warning（AC-6 验证方式锁住 lib doc + `[package].description`
  + Spec 三处双写；本 gate 校验 doc 真的能 build）；如本步只 land
  共享类型 + IkError（拆 (1/2)），暂只校验 `IkError` doc，trait doc
  推到 §P.2.2.e Gate
- §6.7 5 条不变量真宣称保持（**0** 降级 / **0** 升级 / **0** 偏移）

##### P.2.1.f 风险锚点

- **workspace `members` 数组字母序**：插入位置必须在 `robot-health`
  之后、`robot-logging` 之前；如插错位置 → clippy 不 fail（`cargo`
  对 members 顺序不强制）→ 但跟既有 12 crate 字母序约定漂移 →
  Plan-Execute diff #2 登记
- **`Cargo.toml:25` "未来 crate" 注释清理**：删 `robot-kinematics` 注
  释行；漏删 = lint 不 fail 但读者误判"还没落地"。reviewer round
  commit msg / 文件 diff 审计需点名
- **`description` 字段 reviewer 审查重点 #3 锚句**：删除 = AC-6
  三处双写降为两处（lib doc + Spec），降低 design-in 强度 → 阻断
- **`thiserror::Error` derive proc-macro 间接 dep**：`cargo tree -p
  robot-kinematics` 不带 `--edges normal,no-proc-macro` 会列出
  `proc-macro2` / `syn` / `quote` 等 derive 阶段依赖，**不**算 runtime
  dep；`--edges normal,no-proc-macro` 同时排除 build / dev edge + 全
  部 proc-macro crate（AC-1 验证方式已锁）。注意 `--no-dev` 不是独立
  flag —— `cargo tree --help` 输出 `--edges <KINDS>` value 列表里
  `no-dev` / `no-proc-macro` / `no-build` 等都是 KIND 值；旧 Plan
  draft 写的 `--edges normal --no-dev` 命令 cargo 直接 reject

#### P.2.2 [仅 (1/1) 单 commit] OR [commit (2/2)] ArmIkSolver trait + dyn-safety 锁测

##### P.2.2.a `ArmIkSolver` trait 定义

`crates/robot-kinematics/src/lib.rs` append（沿 Spec AC-5 + Q1(A) +
Q6 dyn-safety ACK）：

```rust
/// 笛卡尔目标 → 关节弧度的 IK solver trait。
///
/// # dyn-safety
///
/// 严格 dyn-safe：方法签名不含 generic param（`Self: Sized` bound 除
/// 外）/ 不返 `Self`；可作 `Box<dyn ArmIkSolver>` 存于 S1 coordinator
/// （Q6 ACK；编译期锁测见 `_assert_arm_ik_solver_dyn_safe`）。
///
/// # 多臂支持
///
/// trait **不**暴露 `side` / `arm_id` 字段（Q4(X) ACK）；多臂场景由
/// 调用方持多个 `Box<dyn ArmIkSolver>` instance（每臂一个；跟 dimos
/// `ArmIK(side)` 模式一致）。
///
/// # 错误模型与 clamp 行为
///
/// `IkError::OutOfReach` 在本 trait 层**可选** —— solver 实现**可
/// 以**返 `Ok(IkSolution { joints_rad: clamped_to_workspace })` +
/// 配套外部 informational metric，**也可以**返 `Err(IkError::
/// OutOfReach { distance_m })`；trait crate 不强制语义（XLeRobot 实
/// 施期第二轮 round 拍板）。
pub trait ArmIkSolver: Send + Sync {
    /// 返回 solver 的运动学元数据（关节数 / 名 / limit）。
    ///
    /// trait 层不约束 lazy / eager 内部布局；solver 实现自决。
    fn info(&self) -> &KinematicInfo;

    /// 求解笛卡尔目标到关节弧度。
    ///
    /// # 参数
    ///
    /// - `target`：目标 pose；trait 层**不**钉死 frame 抽象，frame
    ///   语义由 solver 实现 + 配置文档约定（第二轮 round 跟 `IkPayload`
    ///   fbs body 一起拍板，Q12）。
    /// - `seed`：可选 seed（数值法用；闭式解 ignore）。`None` = 无
    ///   seed；`Some(slice)` 长度由 solver 实现按 `info().joint_count`
    ///   校验，违反返 `IkError::InvalidInput { reason }`。
    /// - `deadline_ns`：可选 deadline（绝对 ns from `CLOCK_MONOTONIC_
    ///   RAW`；语义跟 `robot-core::time::MonoRawNs(u64)` + `Deadline`
    ///   + `ActionRequest.deadline_ns: u64` 仓库统一时间模型对齐；
    ///   trait crate 不依赖 robot-core，仅值语义对齐避免 signed
    ///   conversion 损失精度）。`None` = 无 deadline；`Some(ns)` =
    ///   绝对超时 ns；solver 内部自查 + 返 `IkError::DeadlineExceeded`。
    fn solve(
        &self,
        target: &Pose,
        seed: Option<&[f64]>,
        deadline_ns: Option<u64>,
    ) -> Result<IkSolution, IkError>;
}
```

##### P.2.2.b `_assert_arm_ik_solver_dyn_safe` 编译期锁测（AC-2 + Q6）

`crates/robot-kinematics/src/lib.rs` tests 模块 append：

```rust
#[cfg(test)]
mod dyn_safety_tests {
    use super::*;

    /// 编译期 bound check —— 若 `ArmIkSolver` 非 dyn-safe，本函数
    /// 签名编译 fail（沿候选 1 同款 monomorphize 锁测模式）。
    fn _assert_dyn_safe<T: ?Sized + ArmIkSolver>() {}

    #[test]
    fn arm_ik_solver_is_dyn_safe() {
        // 强制 monomorphize `_assert_dyn_safe::<dyn ArmIkSolver>` —— 若
        // trait 引入 `Self` 返回值 / generic method 等 dyn-unsafe 元素，
        // 本行编译 fail（trait object 不可构）。
        _assert_dyn_safe::<dyn ArmIkSolver>();
    }
}
```

##### P.2.2.c ArmIkSolver trait lib unit（AC-5）

最少 1 测（trait object 可构 + `info()` / `solve()` 可调）：

```rust
#[cfg(test)]
mod trait_tests {
    use super::*;

    struct StubSolver {
        info: KinematicInfo,
    }

    impl ArmIkSolver for StubSolver {
        fn info(&self) -> &KinematicInfo {
            &self.info
        }
        fn solve(
            &self,
            _target: &Pose,
            _seed: Option<&[f64]>,
            _deadline_ns: Option<u64>,
        ) -> Result<IkSolution, IkError> {
            Ok(IkSolution { joints_rad: vec![0.0; self.info.joint_count] })
        }
    }

    #[test]
    fn arm_ik_solver_trait_object_callable() {
        let stub: Box<dyn ArmIkSolver> = Box::new(StubSolver {
            info: KinematicInfo {
                joint_count: 3,
                joint_names: vec!["a".into(), "b".into(), "c".into()],
                joint_limits: vec![JointLimit { min_rad: -1.0, max_rad: 1.0 }; 3],
            },
        });
        let target = Pose { translation: [0.0; 3], orientation: [0.0, 0.0, 0.0, 1.0] };
        let sol = stub.solve(&target, None, None).expect("stub solver");
        assert_eq!(sol.joints_rad.len(), 3);
        assert_eq!(stub.info().joint_count, 3);
    }
}
```

##### P.2.2.d 可选：`crates/robot-kinematics/README.md`（reviewer Plan round 拍板）

reviewer Plan round 1 拍板项 #2（详 §P.3）。如拍 (a) 落 README，本
段落到 (1/1) 或 (2/2) commit；如拍 (b) 不落，跳过。

最小 README 内容（沿 Spec §"可选改动"）：
- crate 用途 + first release 状态
- 跟 `docs/00_design.md` §4.1 预写文件结构 (`arm.rs`) 映射
- `ArmIkSolver` 一行 API 摘要
- "不预钉 XLeRobot clamp 行为" 跟 lib doc 同款锚句（reviewer 审查重点
  #3 第三处双写候选 —— 仅 README 落地时双写升级为四处）

##### P.2.2.e Quality Gate（commit (1/1) 全段总 Gate OR (2/2) 增量 Gate）

- `cargo fmt --all --check` clean
- `cargo clippy --workspace --all-targets -- -D warnings` 双路 clean
- `cargo build --workspace --release` 过
- `cargo test -p robot-kinematics --lib`：3-5 测全过（拆 (1/2)+(2/2)
  时 (2/2) 增量 1-3 测；单 commit (1/1) 时合并 ≥ 3 测含 P.2.1.d + 本
  段 ≥ 1）；**含**：
  * `ik_error_variant_exhaustive` PASS（AC-4）
  * `arm_ik_solver_is_dyn_safe` PASS（AC-2 + Q6）
  * `arm_ik_solver_trait_object_callable` PASS（AC-5）
  * `pose_twist_jacobian_pod_constructable` PASS（AC-3 nice-to-have）
- `cargo test --workspace --tests --features "robot-rpc/client" --
  --test-threads=1` 既有 940+ PASS 全保
- `crash_restart_smoke --iterations 5`（real-s1 路径）**5/5 PASS** 硬
  gate（AC-Q；沿候选 1 + 3 同款回归门槛）
- `crash_restart_smoke --use-mock-s1 --iterations 5`（mock-s1 路径）
  既有 baseline regression informational only（AC-Q；本 round 1 不写
  成硬 gate）
- 重型 smoke 回归（`pubsub_cleanup_smoke --phase all` /
  `scripted_backend_smoke --phase all`）**是否进硬 gate** =
  reviewer Plan round 拍板项 #3（详 §P.3）；plan author 倾向 (b)
  降为 informational only（理由：本轮零 runtime 代码路径 + 零 wire
  改动，重型 smoke compile gate + selftest 已足够覆盖；详 §P.3 #3）
- `cargo run -p robot-msg --bin schema_lint` 过（不动 schema_id；既
  有锁测沿用 —— **0** 触）
- `cargo deny check` 过
- `cargo doc -p robot-kinematics --no-deps` 过（无 warning；render
  lib.rs `//!` 段 + `ArmIkSolver` trait doc + `IkError` 5 variant doc
  + `Pose` / `Twist` / `Jacobian` / `KinematicInfo` / `JointLimit` /
  `IkSolution` POD doc；AC-6 验证方式硬 gate —— "clamp 行为不预钉"
  锚句必须在 doc render 后仍可读，详 §P.2.2.a trait doc + §P.2.1.c
  lib doc 段）
- §6.7 5 条不变量真宣称保持

##### P.2.2.f 风险锚点

- **dyn-safety 锁测漏调用 monomorphize**：仅 `fn _assert_dyn_safe<T:
  ?Sized + ArmIkSolver>() {}` 不调用 = 编译过；必须 `#[test] fn` 内
  `_assert_dyn_safe::<dyn ArmIkSolver>()` 强制单态化才真锁；reviewer
  审查重点 #1 落地点
- **`Send + Sync` super-trait 强制**：trait `: Send + Sync` 保 multi-
  thread `Arc<dyn ArmIkSolver>` 共享（v0.4 第二轮 round S1 coordinator
  接入预案需求）；如降为 `Send + 'static` 单线程 → 第二轮 round S1
  设计需重审；本轮**不**降
- **stub solver 测覆盖度**：1 测仅证 "trait object 可构 + 两方法可调"；
  **不**证语义正确性（语义留到第二轮 round XLeRobot impl + e2e
  smoke）；reviewer Plan round 1 如要求增加 "seed 长度校验返
  InvalidInput" 模拟测 → Plan-Execute diff #4 登记
- **`deadline_ns` 未在 stub 验证**：trait 参数语义 "solver 内部自查 +
  返 `DeadlineExceeded`" 由实现负责；trait crate 不强制；本轮 stub
  忽略 `_deadline_ns` 不算回归

#### P.2.3 [commit summary] summary 归档

新文件：`docs/handoffs/v0.4_robot_kinematics_round1_summary.md`（沿
`v0.4_scripted_backend_summary.md` / `v0.4_pubsub_cleanup_summary.md`
同款模板）。

| 段 | 内容 |
|---|---|
| §1 范围 | 候选 2 round 1 = `robot-kinematics` trait crate first release（`ArmIkSolver` trait + 5 共享类型 + `IkError` 5 variant；零 runtime 代码路径；第二轮 round = `robot-kinematics-xlerobot` impl + `IkBackend` + S1 wiring + e2e smoke）|
| §2 commit chain | CodeMap `557e354` + Spec round 1 `64cf312` + 实施 (1/1) 或 (1/2)+(2/2) + summary = 3-4 commit chain（含 hash + 文件统计）|
| §3 关键契约 | (a) trait dyn-safe + `_assert_dyn_safe::<dyn ArmIkSolver>` 编译期锁 ; (b) `IkError` exhaustive match 锁 + 5 variant 不变 ; (c) trait 层**不**预钉 XLeRobot clamp 行为（lib doc + Spec + `[package].description` 三处双写）; (d) 零 robot-s1 / robot-rpc / robot-msg / robot-s0 / robot-adapters / robot-tests 改动 ; (e) `cargo tree -p robot-kinematics --edges normal,no-proc-macro` 直接 dep 仅 `thiserror` ; (f) §6.7 5 条不变量真宣称保持 |
| §4 AC + Quality Gate 实测 | AC-1 至 AC-7 + AC-Q 全过记录 ; `crash_restart_smoke` real-s1 5/5 + mock-s1 informational; 重型 smoke 按 P.3 #3 拍板结果走 |
| §5 §6.7 5 不变量真宣称 | **0** 降级 / **0** 升级 / **0** 偏移 |
| §6 接班入口 | 候选 2 第二轮 round 入口（独立 reviewer round + 独立 Spec：`robot-kinematics-xlerobot` impl + `IkBackend` + S1 wiring + `ik_backend_smoke` cross-process e2e）触发条件 + checklist |
| §7 Reviewer Plan round 归档 | Plan round 1/2/... ACK 节点 + Plan-Execute diff 列表（沿 Spec §P.4）|

同步更新：

- `docs/handoffs/README.md` 加 anchor `v0.4_robot_kinematics_round1_
  summary.md`（沿候选 1 / 3 README 同款手动同步约定）
- `docs/CHANGELOG.md` 加 2026-MM-DD 候选 2 round 1 land 行（trait crate
  first release + `ArmIkSolver` + 零 runtime 改动）
- `CLAUDE.md` 状态段更新：候选 2 round 1 ✅；候选 2 第二轮 round 准备开

### P.3 reviewer Plan round 拍板项

下列项**未**在 Spec round 1 ACK 范围；reviewer Plan round 1 拍板后
锁入 Plan v2 / Plan-Approved，进入实施。

| # | 拍板项 | 选项 | plan author 倾向 | 触发理由 |
|---|---|---|---|---|
| 1 | commit 节奏拆段 | (a) 单 commit (1/1) / (b) 拆 2 commit (1/2)+(2/2) / (c) 拆 3+ commit | **(a)** 单 commit (1/1) | 范围极小（~80-120 行实施代码 + ~50-80 行测）+ append-only 现有 12 crate 0 改动；拆段反增 review 跳转成本 |
| 2 | README 是否进 round 1 | (a) 落 README / (b) 不落 round 1 | **(b)** 不落 round 1 | lib doc + Spec + `[package].description` 三处双写已足够 design-in reviewer 审查重点 #3；README 推到第二轮 round 跟 xlerobot impl + 用法示例一起落（沿候选 1 / 3 同款 README 可选） |
| 3 | 重型 smoke 回归门槛 | (a) `pubsub_cleanup_smoke --phase all` / `scripted_backend_smoke --phase all` 双进硬 gate（沿候选 1 + 3 同款） / (b) 降为 compile gate / selftest only / (c) 完全不跑 | **(b)** compile gate + selftest only（informational only） | 本 round 1 零 runtime 代码路径 + 零 wire schema 改动 + 零 robot-s1/rpc/msg 改动 → 重型 e2e smoke compile pass 即证不退化；full `--phase all` 跑动 overhead 高于收益（沿 Spec AC-Q 非阻断 #2 reviewer "可以留到 Plan 阶段再决定是否降为 selftest/compile gate"） |
| 4 | dyn-safety 锁测形态 | (a) `_assert_dyn_safe::<dyn ArmIkSolver>()` 单测 / (b) 增加 `assert_obj_safe!` macro / (c) 仅 `Box<dyn ArmIkSolver>` 局部变量构造 | **(a)** `_assert_dyn_safe` 单测 | 沿候选 1 monomorphize 锁测同款模式；`assert_obj_safe!`（`static_assertions` crate）= 新 dev-dep，违反 Q3(i) 零依赖原则；trait object 局部构造已 cover by AC-5 stub solver |
| 5 | `crates/robot-kinematics/src/lib.rs` 文件结构 | (a) 单 lib.rs 全集 / (b) 拆 `arm.rs` + `error.rs` + `types.rs` + `lib.rs` 模块树 | **(a)** 单 lib.rs | 代码量 ~80-120 行（含 doc + tests）远未到拆模块阈值；`docs/00_design.md` §4.1 预写 `arm.rs` 文件名是**第二轮 round** xlerobot impl 时的目标结构（详 Spec §"严禁改动"），本轮 trait crate first release 单文件即可 |
| 6 | `[package].description` clamp 锚句精确措辞 | (a) "trait 层不预钉 XLeRobot clamp 行为（AC-6）" / (b) 通用化 "不预钉 fail-safe 策略" / (c) 不带 AC 引用 | **(a)** 含 AC-6 反引用 | reviewer 审查重点 #3 第三处双写；显式 AC-6 反引用让 description 跟 Spec AC 一致漂移；clippy `cargo-incompat` 不 lint `description` 字段，纯文档锚 |
| 7 | summary 文件命名 | (a) `v0.4_robot_kinematics_round1_summary.md` / (b) `v0.4_robot_kinematics_trait_crate_summary.md` / (c) `v0.4_candidate2_round1_summary.md` | **(a)** round1 后缀 | 候选 2 跨两 round，命名需区分 round 1（trait crate）vs round 2（xlerobot + IkBackend）；`_round1_` 后缀比 `_trait_crate_` 更通用（trait crate 是 round 1 的**内容**而非**身份**）|

> Q1-Q10 已在 Spec round 1 ACK，**不**进本 Plan 拍板表（再拍 = 走
> §P.4 Plan-Execute diff）。

#### P.3.3a — 拍板 #3 (b) 具体命令（plan author 倾向方案锁定时落地）

如 reviewer Plan round 1 ACK 拍板 #3 (b)，本轮重型 smoke 回归门槛降为
下列 **5 条命令** —— **不** 跑 `--phase all` full e2e；compile gate +
selftest 跑通即过：

```bash
# (i) candidate 3 pubsub cleanup smoke compile gate
cargo build --release -p robot-tests --bin pubsub_cleanup_smoke \
    --features pubsub-cleanup-smoke

# (ii) candidate 3 pubsub cleanup smoke selftest only（不 bring up
#       s0/s1 子进程，仅验证 gateway hookup + state pub 路径可构）
#       --hardware-id / --config 都是 required；CLI 强校验，详
#       crates/robot-tests/src/bin/pubsub_cleanup_smoke.rs:271-272；
#       config 沿候选 3 summary §"复现命令" 同款用 crash_restart_smoke.toml
./target/release/pubsub_cleanup_smoke \
    --phase selftest \
    --hardware-id smoke_arm \
    --config configs/crash_restart_smoke.toml

# (iii) candidate 1 scripted backend smoke compile gate
cargo build --release -p robot-tests --bin scripted_backend_smoke \
    --features scripted-backend-smoke

# (iv) candidate 1 scripted backend smoke selftest only（沿候选 1 §P.2.3.d
#       "selftest" phase = 自检 gateway 通 + fake S0 publish + scripted
#       payload fbs build → root verify；不跑 happy_multi_waypoint /
#       cancel_mid / payload_boundary 真路径）
#       --hardware-id / --config 都是 required；CLI 强校验，详
#       crates/robot-tests/src/bin/scripted_backend_smoke.rs:255-256；
#       config 沿候选 1 既有 configs/scripted_backend_smoke.toml
./target/release/scripted_backend_smoke \
    --phase selftest \
    --hardware-id smoke_arm \
    --config configs/scripted_backend_smoke.toml

# (v) candidate 1 + 3 binary 内 #[test] 模块不退化（per-bin；--tests
#     只选择 integration test target，**不**覆盖 src/bin/*.rs 里的
#     #[test] —— pubsub_cleanup_smoke.rs 确实有 bin unit；
#     scripted_backend_smoke 当前无 bin unit，本命令至少 compile
#     test harness 不退化，语义比 --tests 准）
cargo test -p robot-tests --bin pubsub_cleanup_smoke \
    --features pubsub-cleanup-smoke
cargo test -p robot-tests --bin scripted_backend_smoke \
    --features scripted-backend-smoke
```

**通过判据**：
- (i) / (iii) 退出码 = 0；产物 `target/release/pubsub_cleanup_smoke`
  + `target/release/scripted_backend_smoke` 存在
- (ii) / (iv) 退出码 = 0；selftest 段日志含各自 binary 内置自检 PASS
  锚句（具体锚句沿候选 1 / 3 既有 `--phase selftest` 实现）
- (v) 两条 `cargo test --bin` 各自 0 fail；`pubsub_cleanup_smoke` bin
  unit 全 PASS（现有 #[test] 模块）；`scripted_backend_smoke` bin
  test harness 至少 compile（当前无 bin unit，本 round 1 **不**追加；
  如未来加 #[test] 模块自动 cover）

**升回 (a) full smoke 触发条件**（沿 §P.4 diff #7）：
- (ii) / (iv) selftest 段日志含 WARN / FAIL 锚句（即使 exit 0）
- 实施期 commit 含任一非预期文件改动（AC-7 `git diff` 审计点名）
- reviewer Plan round 复核期改诉求（amend 后 Plan v2 升 (a)）

### P.4 Plan-Execute diff 登记位置

沿候选 1 / 3 Plan §P.4 同款模板。实施期偏离 Plan 任何决策 → 登记本
节（**不**回退 Plan，appendix-only），含：

- **触发条件**（哪个 Plan 决策被偏离）
- **实际选择**（实施期做了什么）
- **理由**（reviewer ACK / 实施期发现）
- **验证回归**（`crash_restart_smoke` real-s1 5/5 + AC-Q 全过 +
  §6.7 5 不变量真宣称保持）

**预期可能 diff**：

- **diff #1**：lib unit 数量实际 vs 预估 ~3-5（如 reviewer 要求加
  `seed 长度校验 InvalidInput` 模拟 / `OutOfReach` distance_m 非负
  断言 等）
- **diff #2**：workspace `members` 插入位置非字母序（如 reviewer 拍板
  "按落地时间序" 等）；同步 `Cargo.toml:25` "未来 crate" 注释段处理
- **diff #3**：commit 节奏拆段决定如有变化（(1/1) → (1/2)+(2/2) 或反
  向）；实际 hash 跟 §P.1 commit chain 模板差异
- **diff #4**：README 是否补落 round 1（reviewer Plan round 决定 vs
  实施期发现 lib doc + description 双写不够）
- **diff #5**：`cargo deny check` advisory / license exception 新增
  （如 `thiserror` 升级到 v3.x 跨 license 边界等；本 round 沿 v2.0.18
  无变化）
- **diff #6**：Cargo.lock 变化范围跟 AC-7 预期偏离（预期：仅新增
  `[[package]] name = "robot-kinematics"` stanza + `dependencies`
  指向 `thiserror`；`thiserror` / `proc-macro2` / `syn` / `quote`
  等 lock 行**不**应被本轮触动，已被既有 6 crate 共享。如本轮 lock
  生 thiserror minor 版本提升 / proc-macro 生态间接漂移 → diff 登记
  + 审计 reviewer 是否 ACK）
- **diff #7**：重型 smoke 拍板项 #3 实施期回退（如 reviewer Plan
  round 拍 (b) compile gate only 但实施期发现回归 → 升回 (a) full
  smoke 跑）

---

## 后续：reviewer Plan round + 实施

Plan outline draft v1 commit 后，reviewer 进 **Plan round 1** 复核：
- §P.0 工作基线 / §P.1 commit 节奏 / §P.2 工作流（含 §P.2.1.b
  Cargo.toml description 锚句 + §P.2.1.c lib doc 锚句 + §P.2.2.a
  trait doc 锚句三处 design-in 双写）/ §P.2.e Quality Gate 是否够
  严 / §P.3 7 拍板项 plan author 倾向是否 ACK
- 如有阻断 → Plan round 1 amend commit（沿候选 1 Plan round 1/2/3
  amend 同款 commit msg `📝 docs: v0.4 候选 2 Plan round N amend
  (...)`）；零阻断 → ACK 进入实施

**实施 commit 节奏**（拍板项 #1 ACK 后锁；下面按倾向 (a) 单 commit
展开；如拍 (b) 走 §P.1 Happy path B 拆 2 commit）：

```
✨ feat: v0.4 候选 2 round 1 (1/1) robot-kinematics ArmIkSolver
        trait crate first release land
📝 docs: v0.4 候选 2 round 1 summary 归档
```

第二轮 round 入口（独立 reviewer round + 独立 Spec；详 Spec §"后续：
reviewer round 2 + Plan" §"第二轮 round 入口"）：触发 = 本 round 1
summary 归档 + reviewer 第二轮 round entry ACK。
