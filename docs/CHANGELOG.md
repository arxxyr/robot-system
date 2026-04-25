# CHANGELOG

> 本文件记录 `docs/00_design.md` 冻结后的语义讨论与决定，以及 target 文档硬契约的增删改。
> 三类条目：
> 1. **偏离登记**：允许临时偏离设计文档（冻结原则下）；
> 2. **回写登记**：用户拍板后，通过 v0.1.2 "冻结后文档卫生" 机制（附录 E）修改设计文档正文，同步回写 dimos 侧源版本 `docs/09_robot_framework.md`；
> 3. **Target 契约登记**：`targets/<name>.md` §2《训练 / 部署契约》节的新增 / 修改 / 删除；**仅该节**——同文件的 §1 硬件清单 / §3 上游实现参考 / §4 映射 / §5 adapter 需求 / §6 风险 / §7 参考，以及 `_migration.md` 路线图 / `_<subsystem>_<commit7>.md` 源码快照，全部**不走本文件**，PR 直接改。登记规则详见 [`targets/README.md`](targets/README.md)。
>
> 每条记录：日期 + 涉及小节 + 状态（偏离 / 已回写 / target-contract）+ 理由 + 约束 + 影响面 + 审阅人。

---

## 2026-04-21 · 工具链：nightly 硬要求 + edition 2024（**状态：已回写**）

### 决定

- Rust **nightly 是硬要求**，**严格不用 stable**。`rust-toolchain.toml` pin `channel = "nightly"`（可进一步 pin 具体日期 `nightly-YYYY-MM-DD`）。
- **Edition 2024**。`workspace.package.edition = "2024"`。**不写 `rust-version`**：自动跟随最新 nightly；需要复现特定构建时用 `rust-toolchain.toml` 的 `channel = "nightly-YYYY-MM-DD"` pin，而非 `rust-version`。
- 用户（2026-04-21）本 session 明确指令 "严格不用 stable"，据此**回写 `docs/00_design.md` §9**（通过 v0.1.2 "冻结后文档卫生" 机制，附录 E 新增工具链行），并同步回写 dimos 侧源版本 `docs/09_robot_framework.md` §9 + 附录 E。

### 原文 → 当前

原 §9：

> Rust **stable**（尽量不用 nightly）。

当前 §9（见 `docs/00_design.md` / dimos `docs/09_robot_framework.md`）核心句：

- Rust **nightly** 硬要求，不留 stable 回退。CI 不跑 stable 演练。
- 核心 crate（`robot-core` / `robot-msg` / `robot-transport` / `robot-s0` / `robot-s1`）默认不写 `#![feature(...)]`，**目的是降低 nightly 升级回归风险**，不是为了留 stable 后门；确需启用必须 crate 顶部注释 + review。
- 工具类 crate（`robot-cli` / `robot-viz` / `robot-rpc` / `robot-s2-bridge` / `robot-tests` / `robot-recorder`）可自由使用 nightly feature。
- nightly 回归风险靠 **pin 具体日期 + 原地回滚到上一已验证日期** 解决，不切 stable。

### 恒定约束（不变）

1. S0 / S1 RT 铁律（`panic = "abort"`、`mlockall`、禁 heap alloc、禁 `Arc::clone/drop`，`00_design.md` §0 / §4.9）。
2. S0 / S1 禁用 `mimalloc`（`00_design.md` §0 Allocator 策略）—— 与全局 `~/.claude/CLAUDE.md` §5 的 mimalloc 偏好冲突时，本项目优先。
3. Allocator 表按 `00_design.md` §0：外部工具（cli / viz / rpc / s2-bridge）才用 mimalloc。

### 影响面

| 文件 | 调整 |
|------|------|
| `rust-toolchain.toml` | `channel = "nightly"`（Step 0 已创建） |
| `docs/00_design.md` §9 | "stable（尽量不用 nightly）" → "nightly 硬要求 + edition 2024，严格不用 stable" |
| `docs/00_design.md` 附录 E · 冻结后文档卫生 | 追加工具链（2026-04-21）行 |
| dimos `docs/09_robot_framework.md` §9 / 附录 E | 同步回写（源版本） |
| `CLAUDE.md`（项目） | 《Rust 规范》小节重写为 nightly 硬要求表述；workspace 布局注释改引用 `docs/00_design.md §9` |
| `docs/02_start_here.md` | 硬性约束 #4 + Step 0 指示同步 |
| `docs/01_implementation_steps.md` | Step 0 `rust-toolchain.toml` stable → nightly；"不要做的事" #6 改为 "严格不用 stable" |

### 审阅

- 提议 → 决定：用户 loosqk（2026-04-21 本 session 两次明确：先 "使用 nightly"，后 "严格不用 stable"）。
- 复审触发：
  - nightly 连续 3 周破坏核心 crate 编译 → pin 更早日期 + 调查根因，**不切 stable**。
  - 团队扩大 / 引入非 nightly 环境用户 → 重新评估。当前决定视为 v0.x 范围内稳定。

---

## 2026-04-21 · robot-core §4.2 允许列表扩展：加 `libc`（**状态：偏离**）

### 涉及小节

`docs/00_design.md` §4.2 `robot-core` 依赖铁律：

> 允许依赖：thiserror, smallvec, arrayvec, bytemuck, crc32fast

### 决定

Step 1 `robot-core::time` 落地时，`workspace.dependencies` 加入 `libc = "0.2"`，`robot-core/Cargo.toml` 以 workspace 继承方式启用。

### 理由

1. `robot-core::time::SystemClock` 需要 `clock_gettime(CLOCK_MONOTONIC_RAW)` + `nanosleep`（§3.2.1 要求的单调原始时钟），Rust `std`/`core` 均未暴露，必须经 FFI。
2. `libc` 是 `core::ffi` 之上**最薄**的平台常量 + 函数声明层（no_std 友好，零 runtime、零 allocator、零 async executor），与 §4.2 禁止的 `tokio` / `iceoryx2` / `pyo3` / `flatbuffers runtime` / `serde_json` 性质完全不同。
3. 替代路径（手写 `extern "C" { fn clock_gettime(...) }` + 手抄 `timespec` + 手维护 `CLOCK_MONOTONIC_RAW = 4` 的平台差异）反而绕开了 `libc` crate 的跨平台维护，更易出错。
4. 与 §9 "nightly 硬要求 + edition 2024" 属同类"基础约束微调"，此后再有同类需要（例如 `robot-core::os` 对 `sched_setscheduler` / `mlockall` / `sd_notify` 的 FFI）统一走 `libc`。

### 约束

1. **使用范围**：`libc` 仅允许出现在 `robot-core::time`（及未来明确的 OS 抽象模块 `robot-core::os`）。其他模块（`adapter` / `policy` / `task` / `error`）禁止 import `libc`。Step 1 用 module-level `// 允许 libc` 注释标注；CI 后续追加 grep 拦截（`rg 'use libc' crates/robot-core/src/` 结果必须全部命中受控模块）。
2. **no_std 友好**：调用必须包 `unsafe { }` block（edition 2024 默认行为已强制）；不引入 `libc` 的 `std` feature（默认 feature 即含，我们不额外 opt-in）。
3. **平台支持**：`SystemClock` 仅在 `target_os = "linux"` / `target_os = "macos"` 启用真实 syscall；其他 target 下为 `panic` stub，`lib.rs` 顶部注释说明（项目目标平台是 Linux，macOS 为开发期 Step 0–5 支持；Windows 仅 S2 客户端不经 robot-core::time）。

### 影响面

| 文件 | 调整 |
|------|------|
| `Cargo.toml`（workspace 根） | `[workspace.dependencies]` 追加 `libc = "0.2"` |
| `crates/robot-core/Cargo.toml` | `[dependencies]` 追加 `libc = { workspace = true }` |
| `crates/robot-core/src/time.rs` | 新建（Step 1 交付） |
| `docs/00_design.md` §4.2 | **不动**；本条仅作偏离登记 |

### 审阅

- 提议：Claude Code（Step 1 实施前拍板）
- 决定：用户 loosqk（2026-04-21 session 内同意 Step 1 加 libc 的前置决策点）
- 复审触发：若用户同意，可通过 "冻结后文档卫生" 机制（附录 E）回写 `00_design.md` §4.2 允许列表 + 附录 E 行，把本条状态改为 "已回写"。
- 同类未来扩展（`sched_setscheduler` / `mlockall` / `sd_notify` / `pthread_setaffinity` 等 RT 所必需的 syscall）默认复用本条；**非 OS-syscall 性质的新依赖仍须单独登记**。

---

## 2026-04-22 · workspace 布局扩展：新增 IK 层（**状态：已回写**）

### 涉及小节

`docs/00_design.md` §4.1《模块地图》。

### 决定

模块地图新增两类 crate：

1. **`robot-kinematics/`**：trait 层，零 runtime 依赖。定义 `ArmIkSolver` / `BaseKinematics` / `Pose` / `Twist` / `Jacobian` / `IkError`。
2. **`robot-kinematics-<robot>/`**：每种机器人一个实现。目前规划 `xlerobot`（v0.4）、`piper`（v0.6）；用户可 fork 自己的 `robot-kinematics-<mine>` 而不动 s1 / s0 / adapter。

`robot-s1` 新增 `ik/` 子模块持有 `Option<Box<dyn ArmIkSolver>>`，按 Cargo feature（`xlerobot` / `piper` / ...）选具体实现。`tasks/` 新增 `cartesian` 分支走 IK 胶水。

### 理由

1. **用户需求**：v0.4 起用户要给不同机器人接自己的 IK solver（XLeRobot 5DoF 双臂 / 三轮全向，Piper 单臂等），需要一个明确的 trait 层抽象，避免 fork `robot-s1`。
2. **分层正确性**：按 `00_design.md` §1.1，IK 属于 S1 层 ("避障 / WBC / IK")，不属于 S0（关节空间）或 adapter（joint I/O）。把 IK 放在独立 crate 而非 `robot-core`，避免污染 `robot-core` 的零 runtime 依赖铁律（§4.2 允许列表不受影响）。
3. **替换模型**：沿用 adapter 的 Cargo feature + trait object 模式（`00_design.md` §4.5 "inventory 静态注册" 精神一致），build-time 决定，不做 runtime dynamic plugin（与 v0.x 不做 plugin ABI 的原则一致）。
4. **迁移源**：XLeRobot 侧已有现成实现（dimos `src/xlerobot_dimos/kinematics/arm_ik.py` 5DoF IK + `common/units.py` 三轮全向运动学），v0.4 可直接移植为 Rust，作为 trait 的首个落地。

### 关键 trait 约束（v0.3 前定稿，本条只锁 API 形状）

```rust
pub trait ArmIkSolver: Send {
    fn info(&self) -> KinematicInfo;                        // DoF / base_frame / ee_frame
    fn forward(&self, q: &[f64]) -> Pose;                   // FK，不 fallible
    fn inverse(
        &self,
        target: &Pose,
        seed: &[f64],
        deadline: Deadline,                                 // 绑 robot-core::time，避免在 S1 tick 阻塞
    ) -> Result<IkSolution, IkError>;
    fn jacobian(&self, q: &[f64], out: &mut Jacobian);
}

pub trait BaseKinematics: Send {
    fn body_to_wheel(&self, twist: &Twist, out: &mut [f64]);
    fn wheel_to_body(&self, wheel: &[f64]) -> Twist;
    fn integrate_odom(&self, wheel_delta: &[f64], odom: &mut Pose2d);
}
```

- `inverse()` 带 `Deadline`：数值 IK 超时返回 `IkError::DeadlineExceeded`，S1 回落到上一解或进入 DEGRADED。
- `seed` 是上次关节位：多解 IK 保证连续性。
- 错误枚举固定为 `{ NoSolution, OutOfReach, SingularityTooClose, DeadlineExceeded }`。

### 约束

1. **分层铁律**：`robot-kinematics` 不依赖 `iceoryx2` / `tokio` / `pyo3` / vendor SDK；与 `robot-core` 同等"零 runtime"要求。
2. **不进 S0 / adapter**：S0 只收 `JointCommand`（关节空间），adapter 只做 joint I/O；IK 结果必须先在 S1 转成 `JointCommand` 再下发。Cartesian 语义永远不跨过 S1 → S0 边界。
3. **RT 友好**：`ArmIkSolver::inverse()` 调用点（S1 coordinator tick 内）要求 `deadline` 不超过 S1 剩余 tick 预算（<15 ms，§10.1）；超时返回 fallback，不阻塞 coordinator。
4. **build-time 选择，非 runtime plugin**：`robot-s1/Cargo.toml` 用 `[features] xlerobot = ["robot-kinematics-xlerobot"]` 等；一个二进制产物对应一台机器人。
5. **用户扩展路径**：用户 fork 自有 `robot-kinematics-<mine>` crate，在本地 workspace 加 `[patch]` 或独立 workspace 引用，`robot-s1` 以 `default-features = false` + 自定义 feature 挂入。不需要修改上游 `robot-s1` / `robot-s0` / `robot-adapters` / `robot-msg`。

### 影响面

| 文件 | 调整 |
|------|------|
| `docs/00_design.md` §4.1 模块地图 | 新增 `robot-kinematics/` + `robot-kinematics-<robot>/`；`robot-s1` 子目录加 `ik/` 和 `tasks/cartesian` |
| `docs/00_design.md` 附录 E · 冻结后文档卫生 | 追加 "IK 分层（2026-04-22）" 行 |
| dimos `docs/09_robot_framework.md` §4.1 / 附录 E | 同步回写（源版本） |
| `CLAUDE.md`（项目）《规划的 workspace 布局》 | 新增两类 crate + "IK 分层说明"小节 |
| `Cargo.toml` 未来 crate 注释列表 | 追加 `robot-kinematics`（v0.3）/ `-xlerobot`（v0.4）/ `-piper`（v0.6） |
| `docs/01_implementation_steps.md` | 本条**不动**；IK trait 的 Step 放 v0.3 专项规划，届时单独补条目（目前 Step 2-6 不涉及 IK） |

### 审阅

- 提议：Claude Code（用户询问"根据不同机器替换自己的 IK 方便吗"后给出架构补丁）。
- 决定：用户 loosqk（2026-04-22 session 内同意 "现在只做 1) 改布局图 2) 登记 CHANGELOG"）。
- 复审触发：
  - v0.3 定 `ArmIkSolver` trait 时，若发现实际需要 `Box<dyn>` 以外的多态形态（例如泛型 solver 让 S1 coordinator 静态分发），回到本条登记 API 形状变更。
  - v0.6 Piper 异构机器人落地时，若 trait 不足以表达（例如 7DoF 冗余、闭链、可变 DoF），视为"抽象漏洞" → 再补一条 CHANGELOG，届时可能修订 `robot-kinematics` 的 trait。

---

## 2026-04-22 · workspace runtime 依赖白名单机制（**状态：登记/建立约定**）

### 背景

`docs/00_design.md` §4.2 规定了 **`robot-core` 依赖铁律**（允许 `thiserror / smallvec / arrayvec / bytemuck / crc32fast`，禁 `iceoryx2 / tonic / pyo3 / tokio / tracing-subscriber / vendor SDK / flatbuffers runtime / serde_json`，§4.2 只约束 `robot-core`）。

`robot-msg` / `robot-transport` / `robot-s1` 等其他 crate 不在 §4.2 约束范围内，但为防止"顺手加 serde_json / tokio / reqwest"这类依赖漂移，在本 CHANGELOG 登记 **per-crate runtime 依赖白名单约定**，作为 §4.2 机制向整个 workspace 的延伸。

### 约定

1. 每个 crate 首次引入新的 runtime 依赖（非 dev-dep、非 build-dep）前，必须在本 CHANGELOG 追加或更新该 crate 对应的白名单条目，写明：依赖名 / 版本范围 / 理由 / RT 影响 / 使用范围限制。
2. 白名单外依赖 → 视同"偏离"，走标准 CHANGELOG 偏离流程。
3. `dev-dependencies`（`trybuild` / `proptest` / `criterion` 等）**不受此约定约束**，按需添加。
4. 本条是**约定**，不是 00_design.md 回写；§4.2 文字不变。

### 各 crate 白名单（v0.2 开工基线）

#### `robot-core`（§4.2，原文）
- 允许：`thiserror`、`smallvec`、`arrayvec`、`bytemuck`、`crc32fast`
- 例外：`libc = "=1.0.0-alpha.3"`（仅 `robot-core::time` 与未来 `robot-core::os`，见 2026-04-21 条；2026-04-22 由用户决定从 `0.2.x` stable 切到 `1.0` 预发布轨道，精确 pin 防 alpha.N → alpha.N+1 破坏性变更）
- 禁止：`iceoryx2 / tonic / pyo3 / tokio / tracing-subscriber / vendor SDK / flatbuffers runtime / serde_json`

#### `robot-msg`（v0.2 Step 2 登记）
- 允许：
  - `flatbuffers = "=25.12.19"`（与 vendor flatc 25.12.19 对齐；Step 2 schema 反序列化必需。Cargo.toml 用 `=` 精确 pin，防 minor 漂移）
  - `blake3`（`schema_lint` bin 的 `layout_id` 碰撞检测 + `ConfigIdentity` 相关 hash）
- 禁止：同 `robot-core` 禁用列表（flatbuffers runtime 除外）
- RT 影响：`robot-msg` 的 reader 路径零拷贝、零 allocation；`FlatBufferBuilder` 构造**禁止**进 RT thread（§0 RT thread 禁令已收录）

#### `robot-transport`（v0.2 Step 3 首次登记 / Step 5 会追加 iceoryx2）
- 允许（Step 3 已引入，见 `crates/robot-transport/Cargo.toml`）：
  - `robot-msg = { workspace = true }`（解析 FrameHeader / 查 `SCHEMA_REGISTRY`）
  - `robot-core = { workspace = true }`（保留入口；parser 目前未用，但 Step 4
    `SampleLease<T>` / trait 层会用到 `RobotClock` / newtype）
  - `flatbuffers = { workspace = true }`（`flatbuffers::root` verifier + 类型 handle）
  - `thiserror = { workspace = true }`（`ParseError` 分类派生）
- 预期允许（Step 5 追加）：`iceoryx2 = "0.x"`（具体版本在 Step 5 引入时登记 sha256 + tag）
- 禁止：同 `robot-core` 禁用列表，外加 `tokio`（RT 线程会直接走 parser 路径，不得引入 async runtime）
- RT 影响：`parse_sample` **零 alloc、零 panic、零 unwrap**，可在非 RT subscriber 线程调用；RT 线程本身仍不应在 hot path 直接 parse iceoryx2 sample（IPC 线程负责解析后写 slot，§1.2）

#### `robot-kinematics`（v0.3 登记）
- 预期允许：纯 math（`nalgebra`?/自研简薄线代），零 runtime
- v0.3 开工时具体登记

#### `robot-s0`（v0.2 Step 6 登记）
- 允许：`libc`（继承 `robot-core` 例外，RT 线程 syscall 必需）
- 禁止：`tokio` / `mimalloc`（§0 Allocator 策略 + §4.7 Python 隔离的延伸）
- Step 6 具体登记 `iceoryx2` 依赖路径（经 `robot-transport` 间接）

#### `robot-s1`（v0.3 登记）
- 预期：`tokio`（只在 non-RT 协调线程）；具体边界 v0.3 落地时登记

#### `robot-policy-host`（v0.5 登记）
- 预期：`pyo3` / `ort`（ONNX runtime）；v0.5 具体登记

#### 工具类 crate（`robot-cli` / `robot-viz` / `robot-rpc` / `robot-s2-bridge` / `robot-tests` / `robot-recorder`）
- 宽松策略：依赖加入时 CHANGELOG **仍需登记**，但不做"允许/禁止"二分；RT / 安全相关除外
- 例如 `robot-rpc`: `tonic` / `tower` / `prost`；`robot-viz`: `rerun-sdk` / `foxglove`；`robot-s2-bridge`: `pyo3`

### 影响面

| 文件 | 调整 |
|------|------|
| `docs/00_design.md` §4.2 | **不改**；本条是向其他 crate 的机制延伸，不修订 `robot-core` 铁律 |
| dimos `docs/09_robot_framework.md` §4.2 | **不改**（源文档同上） |
| `docs/CHANGELOG.md`（本文件） | 新增本条；未来每次新 crate / 新依赖都在此补登记 |
| `docs/01_implementation_steps.md` Step 2 交付 | 显式列出 `flatbuffers = "=25.12.19"` 并指向本条 |

### 审阅

- 提议：Claude Code（Step 2 开工前决策 #3）。
- 决定：用户 loosqk（2026-04-22 session 内同意 "CHANGELOG 顺手登记一条 robot-msg 的 runtime 依赖白名单"）。
- 复审触发：
  - 任何 crate 首次引入新 runtime 依赖 → 更新本条对应 crate 子清单。
  - 若条目数量超过一页可读范围 → 把白名单拆到单独 `docs/dependency_allowlist.md`，本条仅保留约定本身。

---

## 2026-04-22 · Step 2 工具链决策（**状态：实施细节登记**）

### 涉及 Step

`docs/01_implementation_steps.md` Step 2 · `robot-msg` schema registry。

### 决定（三条）

1. **flatc vendor 策略：A 预编译二进制**
   - 路径：`3rd-party/flatc/{darwin-arm64,linux-x86_64,linux-aarch64}/flatc` + `VERSION` + `checksums.sha256`
   - 版本 pin `flatbuffers 25.12.19`，与 Rust `flatbuffers = "=25.12.19"` 对齐（`=` 精确 pin，防 minor 漂移）
   - 拒绝 B（源码 + cmake 构建）：CI 每次 clean build 多 2–3 min 不可接受，且 flatc 升级频率 ≪ 1/年
   - 升级走手动 `scripts/refresh_flatc.sh` + `git commit`，不在 CI 自动拉新版本

2. **Schema lint：独立 Rust bin**
   - 位置：`crates/robot-msg/src/bin/schema_lint.rs`
   - 调用：`cargo run -p robot-msg --bin schema_lint`
   - 分工：
     - `tests/compat.rs` → 运行时 round-trip / old-new reader 交叉（`cargo test`）
     - `schema_lint` bin → 静态校验 deprecated id / schema_id 唯一 / layout_id 碰撞 / 字段 id 单调
   - v0.2 实现：regex 扫 `.fbs` 文本；v0.3 视需要升级到 `bfbs` 反射
   - **拒绝 Python 脚本**：全局 CLAUDE.md 偏 Rust/C++，CI 不引入 Python runtime

3. **依赖白名单扩展到 workspace**：`flatbuffers = "=25.12.19"` 作为 `robot-msg` 的第一条白名单入库，见本文件 "workspace runtime 依赖白名单机制" 条目

### 影响面

| 文件 | 调整 |
|------|------|
| `docs/01_implementation_steps.md` Step 2 交付 / 测试 / 退出标准 | 改写 vendor 布局、`schema_lint` bin、`refresh_flatc.sh`、退出标准加 "无网可 build / 无 Python 环境" |
| `3rd-party/flatc/` | Step 2 开工时创建目录结构 + 跑 `scripts/refresh_flatc.sh` 拉取二进制（用户本地一次性操作） |
| `scripts/refresh_flatc.sh` / `refresh_flatc.ps1` | Step 2 开工交付 |
| `docs/00_design.md` §9 / §4.1 | **不改**（实施细节，不是架构语义） |

### 审阅

- 提议 + 决定：用户 loosqk（2026-04-22 session 内明确同意 A 预编译 + 独立 Rust lint bin + 白名单登记）。
- 复审触发：
  - flatc 新版本出现关键 schema feature → 走手动升级流程（`refresh_flatc.sh` + sha256 更新）。
  - `schema_lint` 的 regex 策略出现误报 / 漏报 → v0.3 升级到 bfbs 反射。

---

## 2026-04-22 · §4.3 writer API 调整：`publish` 挂到 writer（**状态：已回写**）

### 原文 → 当前

原 `docs/00_design.md` §4.3：

```rust
pub trait Publisher<T> {
    fn loan(&mut self) -> Result<SampleWriter<'_, T>, TransportError>;
    fn publish(&mut self, w: SampleWriter<'_, T>) -> Result<(), TransportError>;
}
```

当前（§4.3 + 附录 E 冻结后文档卫生行）：

```rust
pub trait Publisher<T> {
    fn loan(&mut self) -> Result<SampleWriter<'_, T>, TransportError>;
}

impl<'a, T> SampleWriter<'a, T> {
    pub fn publish(self) -> Result<(), TransportError>;
}
```

### 理由

`Publisher::publish(&mut self, w: SampleWriter<'_, T>)` 在 Rust borrow checker 下不自洽：

1. `loan` 返回 `SampleWriter<'a, T>`，其 `'a` 绑定 `&'a mut Publisher`；
2. 只要 `w` 存活，`&mut Publisher` 借用就被占用；
3. 再调 `Publisher::publish(&mut self, ...)` 要拿第二个 `&mut self` → 冲突。

把 `publish` 挂在 writer 上，**按值消费 self**：writer move 进 `publish` 后自然释放 `&mut Publisher` 借用，publisher 可以立即被再次借用（再次 `loan()`）。语义完全等价。

### 语义不变的约束（§4.3 原保证全部保留）

1. `loan()` 返回的 `SampleWriter` 持 iceoryx2 sample 可写借用，生命周期绑 publisher 的借用期。
2. **未 publish 即 drop**：自动 discard / release loan，不留悬挂 sample（drop impl 实现）。
3. **publish 消费 writer**：调用后 `w` 不可再用；编译期拦截 "publish 后继续写 body/blob"（move semantics）。
4. 错误路径（body 编码失败等）`return Err` 让 writer drop，loan 自动归还。

### 影响面

| 文件 | 调整 |
|------|------|
| `docs/00_design.md` §4.3 | `Publisher` trait 删 `publish` 方法，加独立 `impl SampleWriter::publish(self)` 示例 |
| `docs/00_design.md` 附录 E · 冻结后文档卫生 | 追加 "§4.3 writer API（2026-04-22）" 行 |
| dimos `docs/09_robot_framework.md` §4.3 / 附录 E | 同步回写（源版本） |
| `crates/robot-transport/src/traits.rs` | `Publisher` trait 只有 `loan`；无 `publish` method |
| `crates/robot-transport/src/writer.rs` | `impl<'a, T> SampleWriter<'a, T> { pub fn publish(self) -> ... }` |

### 审阅

- 提议：Claude Code（Step 4 决策 #5，发现 §4.3 原 API 在 borrow checker 下不可实现）
- 决定：用户 loosqk（2026-04-22 session 内同意 "按 ▶ 倾向推"）
- 复审触发：若未来某后端（Zenoh / 裸 shm）的 API 迫使 publish 回到 publisher trait（例如需要 publisher 内部持有的事务性状态在 publish 时更新），重新评估本调整；Step 5 iceoryx2 实现应能直接采纳当前签名。

---

## 2026-04-22 · Step 1 retrofit：`robot-core::time` 补时间代数原语（**状态：登记 / 向前兼容**）

### 涉及 Step

`docs/01_implementation_steps.md` Step 1 · `robot-core::time`（已完成于 `654d7ab`，本条属 retrofit，不回滚 Step 1 状态）。

### 决定

`crates/robot-core/src/time.rs` 追加三个 const fn method，填补 Step 1 原交付清单未覆盖但后续"多传感器时间对齐 / `TimeAligner`"必需的时间代数原语：

```rust
impl MonoRawNs {
    /// `self - other` 的有符号差（ns）。正值 = self 更晚。
    /// i128 中间值防 u64 减法下溢；饱和到 i64（±~292 年 ns，v0.x 绰绰有余）。
    pub const fn signed_delta_ns(self, other: MonoRawNs) -> i64;

    /// |self - other|，方向无关，内部走 u64::abs_diff。
    pub const fn abs_diff(self, other: MonoRawNs) -> DurationNs;
}

impl Deadline {
    /// 距 deadline 剩余时长；已过期返回 None。
    /// 语义与 `Sub for MonoRawNs`（deadline - now）一致。
    pub const fn remaining(self, now: MonoRawNs) -> Option<DurationNs>;
}
```

### 理由

1. **`Sub for MonoRawNs` 只给前向差**（`a >= b` 返 `Some(DurationNs)`，否则 `None`），表达不了"b 比 a 早多少 ns"这种方向信息。`TimeAligner` 的最近邻搜索 / 双向容差判断需要方向，单独开 `signed_delta_ns` 避免每个调用方手工拼 `if a>=b { ... } else { ... }`。
2. **`abs_diff` 是 `TimeAligner` 容差比较的原子操作**（`abs_diff <= tol`）。直接走 `u64::abs_diff` 比手写 if/else 更短、clippy `manual_abs_diff` 零告警。
3. **`Deadline::remaining` 把"距 deadline 还剩多久"从调用方散落的 `deadline.as_mono_raw_ns() - now` 收敛到一处**，const fn + `checked_sub`，零 panic。§6 / §10.1 里所有"剩余预算"语义都该走这个 API。
4. **与 2026-04-22 session 对融合层评估的连续性**：本 session 评估了 "传感器数据根据时间融合吗"（结论：架构只给了时间戳基础设施，没给融合算法层；TimeAligner 该住独立 `robot-fusion` crate，v0.3+ 再排），`robot-core::time` 补这三个方法是**零依赖、向前兼容**的自然延伸，不承诺启动 fusion 工作，但未来谁写 aligner 都少造一次轮子。

### 约束

1. **纯扩展**：signature 向后兼容，不改现有任何 API；原有 33 测试全绿。
2. **零新依赖**：不引入 `nalgebra` / `num-traits` 等 math crate，仍只依赖 `libc`（§4.2 既有例外）。
3. **不 panic / 不 alloc**：三个方法都是 const fn + 饱和 / checked 算术，符合 RT 铁律（§0）；本身仍不建议在 RT thread 直接调用（`robot-core::time` 任意 API 在 RT 路径的使用仍按 §1.2 单独审）。
4. **使用范围**：任意 crate 可用；`TimeAligner` / fusion 层真正落地仍走 CHANGELOG 新条目（届时登记 crate 名 / 依赖 / 策略枚举）。

### 影响面

| 文件 | 调整 |
|------|------|
| `crates/robot-core/src/time.rs` | `impl MonoRawNs` 加 `signed_delta_ns` / `abs_diff`；`impl Deadline` 加 `remaining`；追加 6 条单元测试（含 `u64::MAX` 极限 / 方向对称 / 与 `Sub` 一致性） |
| `docs/00_design.md` §3.2 / §3.2.1 | **不改**（属同一语义域的工具函数扩展，不新增时钟域 / 不引入 wall clock） |
| `docs/01_implementation_steps.md` Step 1 交付清单 | **不改**（retrofit 不回写原步骤；未来整理 v0.2 总结时可并入） |

### 审阅

- 提议：Claude Code（2026-04-22 session 评估 "传感器数据根据时间融合吗" 的后续行动项 #1）
- 决定：用户 loosqk（2026-04-22 session 内 "先把 1 完成"）
- 复审触发：
  - v0.3 抽 `robot-fusion` crate 时，若发现需要有符号 `DurationNs` / `SignedDurationNs` 类型（例如 IMU 样本时间戳可能早于 tick 的 ±1 ms 窗口），考虑把 `signed_delta_ns` 返回值升级为 newtype。
  - 若后续要加 wall clock offset / 硬件时钟校准原语（`HwClockOffset`），单独登记新条目，不塞回本条。

---

## 2026-04-22 · Step 5 开工决策与 iceoryx2 依赖登记（**状态：登记**）

### 涉及 Step

`docs/01_implementation_steps.md` Step 5 · mock iceoryx2 pub/sub。

### 前置：macOS 烟测

在 `/tmp/iceoryx2_smoke` 跑 iceoryx2 0.8.1 独立 sandbox，验证了同进程 pub/sub、ServiceName 字符集、service attribute、event service 四条。macOS (Darwin 25.4) 全绿。烟测源码不入仓库。

### 决策一览（用户 2026-04-22 session 拍板）

| # | 决策 | 结论 |
|---|---|---|
| ① iceoryx2 版本 pin | `iceoryx2 = "=0.8.1"` 精确 pin，仿 flatbuffers `=25.12.19`；升级走 CHANGELOG |
| ② Service name 映射 | **方案 A**：topic 原样喂 `ServiceName::new()`（`/<robot>/s0/<hw>/state` 实测接受），§2.3 命名空间零改动 |
| ③ QoS 映射表（§2.4 → iceoryx2 builder） | 见本条"QoS 映射"小节 |
| ④ Sample size 分档 | 4 档 + 硬编码 default + TOML override 两层，见"size tier"小节 |
| ⑤ schema_id 绑定 | FrameHeader 字段（已落 Step 3）+ iceoryx2 service attribute `schema_id` 两道防线；attribute 是硬约束，不匹配 verifier 拒绝 open |
| ⑥ macOS 可行性 | ✓ 通过烟测；Step 5 可在 macOS 完整做 |
| ⑦ 事件 / 轮询 | 方案 B：`s0_ipc_thread` 用 iceoryx2 Event service + `timed_wait_one` 代替 polling（50 ms 实测 51 ms，抖动 1 ms 内） |

### Size tier（决策 ④ 展开）

**4 档上限**：

| 档 | `max_sample_size` | 覆盖（按当前注册的 9 个 schema） |
|---|---|---|
| control | **8 KiB** | JointLayout / JointState / JointCommand / Twist / EStopEvent / Heartbeat |
| sensor_small | **1 MiB** | 预留：未来 IMU / FT 等小 sensor |
| sensor_large | **16 MiB** | ImageHeader / PointCloudHeader / TensorHeader |
| tensor | **128 MiB** | 预留：未来 `SCHEMA_POLICY_OBSERVATION` / `SCHEMA_ACTION_CHUNK` |

**两层映射**：

1. **硬编码 default**（`crates/robot-msg/src/size_tier.rs`）：
   - `pub enum SizeTier { Control, SensorSmall, SensorLarge, Tensor }`
   - `pub const fn default_tier_of(schema_id: u32) -> Option<SizeTier>`
   - 未注册 schema 返回 `None`（transport 侧 fail-fast）
   - 单元测试穷举 `SCHEMA_REGISTRY`，每个 id 必须返回 `Some`（加新 schema 必同步加 tier，测试拦截遗漏）

2. **TOML override**（`robot-s0` / `robot-s1` 启动期读 `configs/<robot>.toml`）：
   ```toml
   [transport.tier_override]
   "/xlerobot_01/policy/pi0_5/observation" = { max_sample_size_bytes = 33554432 }  # 32 MiB
   ```
   - 以 **topic 字符串** 为键（最常见场景）；schema 级 override 留未来需要时再加
   - 双向允许（升 / 降）；iceoryx2 在 sample 超限 loan 时自然失败，安全网本来就在
   - 所有 override 参与 `ConfigIdentity.transport_config_hash`，跨节点不一致启动拒绝（具体落地在 v0.3 ConfigIdentity 专项）

### QoS 映射（决策 ③）

§2.4 九类 → iceoryx2 `service_builder.publish_subscribe::<T>()` 选项：

| 类别 | 典型 topic | `history_size` | `subscriber_max_buffer_size` | `max_publishers` | `max_subscribers` | 备注 |
|---|---|---|---|---|---|---|
| S0 command | `/<r>/s0/<hw>/command` | 1 | 1 | 1 | 1 | 过时由 S0 按 `on_timeout` 兜底；不启用 safe_overflow（满了即 drop-and-metric） |
| S0 state | `/<r>/s0/<hw>/state` | 2 | 2 | 1 | N | drop old OK；safe_overflow=true |
| S1 trajectory | `/<r>/s1/trajectory/<task>` | 8 | 16 | 1 | N | 丢弃必报警；safe_overflow=false |
| S1 cmd_vel | `/<r>/s1/cmd_vel` | 1 | 1 | 1 | 1 | latest-only；safe_overflow=true |
| Sensor / Image | `/<r>/sensors/**` | 2 | 4 | 1 | N | drop old；safe_overflow=true |
| Safety event | `/<r>/events/estop` | 32 | 64 | N | N | 不丢；满则升 fault + emergency log |
| Task event | `/<r>/s1/task_status` | 64 | 64 | 1 | N | 丢弃必可观测（metric + log） |
| RPC response | iceoryx2 req/resp | 1 | 1 | — | — | deadline 驱动；不在 publish_subscribe |
| Policy observation / action_chunk | `/<r>/policy/<name>/**` | 1 | 2 | 1 | 1 | VLA 单方向；S1 永不等 policy |

**QoS 推迟项**：payload_alignment（当前统一 8 B；图像 blob 需 64 B 的话 v0.3 再加）、`max_loaned_samples`（默认即可）。

### 事件服务（决策 ⑦）

- `s0_ipc_thread` 订阅 `/<r>/s1/heartbeat` 的 Event service，阻塞在 `Listener::timed_wait_one(2 × heartbeat_period)`
- `s0_pub_thread` 仍周期性 publish，不走 event（publish 是主动动作）
- `/events/estop` 类：走 event + publish 双通道（event 通知 wakeup，publish 带载荷）
- Step 5 先落 publish_subscribe，Event service wrapper 放 Step 5 末段或 Step 6 初段

### 影响面

| 文件 | 调整 |
|---|---|
| `Cargo.toml`（workspace 根） | `[workspace.dependencies]` 追加 `iceoryx2 = "=0.8.1"` |
| `crates/robot-transport/Cargo.toml` | `[dependencies]` 可选 `iceoryx2 = { workspace = true, optional = true }`；`[features]` 加 `iceoryx2 = ["dep:iceoryx2"]`；默认 feature 从 `["mock"]` 改 `["mock", "iceoryx2"]`（macOS 烟测已验证） |
| `crates/robot-msg/src/size_tier.rs` | 新增：`SizeTier` / `default_tier_of` + 穷举测试 |
| `crates/robot-msg/src/lib.rs` | 导出 `size_tier::*` |
| `crates/robot-transport/src/iceoryx2/` | 新增：`mod.rs` / `service_name.rs` / `qos.rs` / `publisher.rs` / `subscriber.rs` / `attributes.rs`（Event wrapper 酌情） |
| `docs/01_implementation_steps.md` Step 5 | 交付清单具体化（后续 commit） |
| **`docs/00_design.md`** | **不改**。QoS 映射 + size tier 是实施细节；§2.4 类别定义不变 |

### 约束

1. **iceoryx2 使用范围**：仅 `robot-transport::iceoryx2` 子模块。其他 crate（`robot-msg` / `robot-s0` / `robot-s1` / adapters）禁止 `use iceoryx2`。CI 后续加 grep 拦截。
2. **Service attribute 键空间**：目前只用 `schema_id` / `schema_version`；未来增加 `layout_id` 等需登记新条目。
3. **Config 路径的 override 未在 Step 5 落地**：Step 5 只落 default tier；override 机制推到 v0.3 ConfigIdentity 专项时一起做。

### 审阅

- 提议：Claude Code（Step 5 开工决策，含 macOS 烟测）
- 决定：用户 loosqk（2026-04-22 session 内逐条拍板：①⑥ 烟测通过 → ② 从 B 改回 A；④ 4 档 + 两层 + control 升 8 KiB；⑤ 先看 C 成功 → C；⑦ B）
- 复审触发：
  - iceoryx2 0.9.x / 1.0 发布时重跑烟测 → 更新精确 pin
  - 出现单个 topic 实际需要穿越档位（含升降档）→ 走 TOML override 而非扩充默认档
  - macOS 烟测 regression（iceoryx2 新版 macOS 支持回退）→ 撤回 §3.1 默认 feature，`mock` 回到默认

---

## 2026-04-24 · Target 契约登记机制 + XLeRobot 建 target 骨架（**状态：target-contract · 新增登记类别**）

### 涉及小节

- 本文件序言（新增第 3 类条目："Target 契约登记"）
- `docs/README.md` Spec 分工表（Target spec 行范围精化）
- `docs/targets/README.md`（登记规则表 + 优先级表）
- `docs/targets/xlerobot.md`（新增，骨架）
- 不动 `docs/00_design.md`（target 本就不进设计文档）

### 背景

v0.1.2 冻结设计只管 framework 语义。随 `targets/` 目录落地（2026-04-23 新增 atom01.md）和 2026-04-24 重构（契约 / 参考 / 路线图三分），出现一个问题：**target 文档里的"训练 / 部署硬契约"** 一旦改动，后果与改 framework 语义相当（破坏会直接导致机器人抽搐 / 起飞），但本文件此前只登记 framework / design 偏离，没有收口 target 契约变更。

同时，原 `targets/README.md` 优先级表把 XLeRobot 标为"暂不独立成 target spec"（理由：现役栈在 dimos、指针已在 `02_start_here.md`）。这不对称：ATOM01 作为 v0.5 候选已建 target，XLeRobot 作为 v0.4 首个 adapter 落地目标反而没有，无法让 framework 和 target 两侧对齐演进。

### 决定

1. **本文件序言**从"两类条目"扩成"三类条目"，新增第 3 类：**Target 契约登记** —— 仅覆盖 `targets/<name>.md` §2《训练 / 部署契约》节的新增 / 修改 / 删除。
   - 同文件其他节（§1 硬件清单 / §3 上游实现参考 / §4 映射 / §5 adapter / §6 风险 / §7 参考）**不进本文件**，PR 直接改。
   - 辅助文件（`_migration.md` 路线图 / `_<subsystem>_<commit7>.md` 源码快照）**不进本文件**。
   - 详细规则表见 `targets/README.md`《Spec 登记规则》。

2. **XLeRobot 建 target 文档**：新增 `targets/xlerobot.md`，上游参考 `~/repo/robot/dimos`（Python + dimos 框架），按 6 节骨架组织。
   - v0.4 前是骨架态（§2 契约大量 TBD 项，标注"v0.4 开工时填实"），不阻塞开工；
   - 已知的硬契约（Feetech STS3215 寄存器序列 / 三轮全向运动学 / 校准 offset 持久化）先写进去，后续 v0.4 时补完整数值；
   - 撤回 `targets/README.md` 此前"XLeRobot 不补独立 target 文档"的说明。

### 理由

- Target 契约破坏的后果 = framework 不变量破坏的后果（物理后果，不是软件 bug）。既然 framework 走 CHANGELOG，target 契约也应该走，**统一入口**。
- 只登记 §2 契约节是精确边界：路线图 / 源码快照 / 硬件清单变动节奏快，全收到 CHANGELOG 会淹没 framework 条目。
- XLeRobot 建骨架比"等 v0.4 再建"更好：v0.4 实施时发现契约冲突就晚了，先把骨架和 TBD 清单亮出来，framework v0.3 阶段就能感知到 target 侧要什么。

### 约束

1. **Target 契约登记条目的状态标签用 `target-contract`**，不混用 `偏离` / `已回写` —— 因为这类条目不触发回写 dimos 侧设计文档（dimos 侧没有对应 target 契约定义）。
2. **`targets/README.md` 的《Spec 登记规则》表是权威**：本文件序言只引用，不复述规则。
3. **XLeRobot 骨架里的 TBD 项在 v0.4 开工填实时**，每填一条都走一次本文件（target-contract 登记）。
4. 不动 `docs/00_design.md` —— target 文档本就不进设计冻结范围（`00_design.md` 附录 B 只列 dimos ↔ robot-core v0.1.2 对应关系，不规定 target 硬契约）。

### 影响面

| 文件 | 调整 |
|---|---|
| `docs/CHANGELOG.md` 序言 | 两类条目 → 三类条目；新增 "Target 契约登记" 说明 + 指向 `targets/README.md` |
| `docs/CHANGELOG.md`（本条） | 新增本条目 |
| `docs/README.md` Spec 分工表 | Target spec 行本就限定 §2（2026-04-24 上午已改）；本次新增指向"登记规则表" |
| `docs/targets/README.md` | 优先级表 XLeRobot 一行：文档列从"见 §11.3（暂不独立成 target spec）" → `[xlerobot.md](xlerobot.md)`；撤回相应说明段 |
| `docs/targets/xlerobot.md` | **新增**。6 节骨架；§2 大量标 TBD |
| `docs/02_start_here.md` 上游参考段 | 不动；`xlerobot.md` 里重新整理成 target 视角的指针即可 |
| `docs/00_design.md` | **不动**。设计冻结 |

### 审阅

- 提议：Claude Code（2026-04-24 session 内根据用户"深入交流 target 文档"建议整合）
- 决定：用户 loosqk（2026-04-24 session 内明确："targets/ 进 docs/CHANGELOG.md 登记，仅契约。dimos 也进 target"）
- 复审触发：
  - 第二个真 target（Piper 立项 / 四足）落地，发现 6 节骨架或 §2 契约格式需调整 → 本文件登记一次骨架修订，再统一迁移已有 targets；
  - XLeRobot v0.4 开工，§2 TBD 项全部填实后 → 状态从"骨架"转成"v0.4 实施 spec"，同时追加一条 target-contract 条目登记填实范围；
  - 发现 `_migration.md` 路线图步骤改动也会导致契约解释歧义（实践检验） → 评估是否把"路线图重大调整"也纳入本文件。

---

## 2026-04-24 · S0 `s0_rt_thread` 多 bus 语义澄清 + HardRT 线程拓扑方案 B（**状态：登记 / 候选回写 §1.2**）

### 涉及小节

- `docs/00_design.md` §1.2 "S0 四线程模型"（候选回写；本条先登记，不改冻结正文）
- `docs/00_design.md` §4.9 "RT 铁律"（不动，语义已兼容方案 B）
- `docs/targets/atom01.md` §5.1（同步更新 RT 风险表）
- `docs/targets/xlerobot.md` §5.1（XLeRobot 侧 feetech 多总线同规则）
- `CLAUDE.md`（项目）"S0 线程模型（v0.2 骨架）"小节
- `RtSnapshotWriter` API（新增 per-bus slice 维度，v0.4 多 bus 开工前定形）

### 背景

`docs/00_design.md` §1.2 描述 S0 线程模型时写的是单数 `s0_rt_thread`：

```
s0_ipc_thread    普通优先级
s0_rt_thread     SCHED_FIFO    读 command slot → HardRT adapter → 写 rt_snapshot
s0_pub_thread    普通优先级
s0_io_thread     普通优先级
```

v0.2 Step 6 单 bus 场景下字面成立。但 ATOM01（4 × CAN）和 XLeRobot（2-3 × USB-RS485）都是多 HardRT bus；今天（2026-04-24 session）通读 `~/repo/robot/atom01_deploy` 上游 C++ 实现后，发现原栈 `src/motors/src/protocol/can/socket_can.cpp` 采用**每 bus 2 个 SCHED_FIFO prio=80 线程（RX 异步 `select()` + TX 阻塞 cv + Boost lockfree queue）+ 上层 `ThreadPool(4)` fan-out**，4 bus 膨胀到 13 个 RT 线程。且 `src/motors/src/drivers/dm/dm_motor_driver.cpp:135-160` 的 RX callback 在 I/O 线程里直接 `logger_->error(...)` format + 写非 atomic `float` 成员，**既违反 §4.9 RT 铁律，也有数据竞态**。不能作为 robot-system 的参考实现。

用户（loosqk）本 session 明确自己 EtherCAT 背景的模式：**单 RT 线程 [read → update state queue → read target queue → safety → write bus]**。EtherCAT 单 cycle 一帧遍历 ring，PDO 天然 1:1 同步；CAN 是多帧 bus，每电机一帧，RX 异步到达。但只要在 tick 开始做 non-blocking RX drain，EtherCAT 的单线程闭环就能迁移到 CAN。

### 决定

1. **多 bus 场景下，S0 `s0_rt_thread` 从"单例"重释为"每 HardRT bus 一实例"**。单 bus 场景（v0.2 Step 6 mock、Step 10 Feetech 单关节）仍然 M=1，与冻结正文字面兼容。
2. **每 rt_thread 的 tick 闭环**（方案 B）：

   ```
   loop {
       clock.sleep_until(deadline); deadline += tick_period;
       bus.drain_rx();                          // non-blocking，到 EAGAIN 停
       cmd = target_slot.load_latest();         // SampleLease，latest-wins
       if let fault = safety_check(&state, &cmd) {
           bus.safe_output(); snapshot.publish_fault(fault); continue;
       }
       bus.write_frames_serial(&cmd, &state);   // N 次 write()，稳态 <10 µs/次
       snapshot.write_bus_slice(bus.id, &state);
   }
   ```

3. **`RtSnapshotWriter` 加 per-bus slice 维度**：每个 rt_thread 只写自己 slice（seqlock），消费者（pub_thread / s1 coordinator）读多 slice 组装全关节快照。跨线程锁 0 个。具体 API 形状 v0.4 多 bus 开工前定形，v0.2/v0.3 保持单 slice 等价实现。
4. **跨 bus compute（闭链解耦 / IK / 运动学）不进 rt_thread**，挪 `robot-s1` coordinator（§1.3），保 §4.9 RT 铁律。
5. **write() 异常处理**：`EAGAIN` → 记 metric + 跳过该帧，rt_thread 不 retry；恢复策略上抛 s1 / 业务 app。`SO_SNDBUF` / `txqueuelen` 配足是前提（`SO_SNDBUF ≥ 1 MB`，`txqueuelen ≥ 1000`），否则是配置 bug。
6. **CPU affinity**：每 rt_thread 绑独立核（避免 SCHED_FIFO 跨核迁移抖动），atom01 证实过的 defensive pattern 保留。

### 理由

- **契合 §1.2 语义**：§1.2 的 rt_thread 职责（读 target → 调 adapter → 写 snapshot）本身就是方案 B 的描述，只是从单例 → M 实例。不破坏冻结语义，只澄清多 bus 行为。
- **RT 铁律更容易守**：单线程闭环，无 async callback / 跨线程 format / 无锁队列；atom01 踩过的 spdlog-in-callback 坑自动规避。
- **1 kHz 够用**：单 bus 稳态 ~60 µs write + ~100 µs drain，<1 ms tick 裕度 10×。4 bus 并行跑 1 kHz 需要 ~4 个独立核，Orange Pi 5+（RK3588 8 核）/ Orin 8+ 核够。
- **比 atom01 简 8 倍线程**：13 → 4（ATOM01），13 → 2 或 3（XLeRobot），抖动源显著减少。
- **用户 EtherCAT 经验可复用**：只加"drain RX 前缀"，其他结构一一对应。

### 约束

1. **HardRT adapter trait 按"单 bus 单实例"设计**，不假设能调度多 bus；M bus = M adapter 实例。
2. **所有跨线程状态只走 seqlock / atomic / SampleLease**；rt_thread 共享成员不得是非 atomic 基础类型。
3. **spdlog / tracing-format / 任何 format 参数构造不进 rt_thread**（含入站帧处理内联路径），只能 `AtomicU64::fetch_add` 计数，dump 由 pub_thread 完成。
4. **不在 rt_thread 里 spawn 任务或调 ThreadPool**；bus 内串行写 N 帧就够。
5. **watchdog / `sd_notify` 超时检测保留**：rt_thread 卡 > 2 tick → systemd 杀进程 → 硬件 command timeout 兜底（Feetech 100 ms / DM 类似）。
6. **具体 per-bus slice snapshot API 形状推迟到 v0.3 末 / v0.4 初**（ATOM01 / XLeRobot 第一次真多 bus 落地时）；v0.2 Step 6-10 仍 M=1，本条不阻塞当前实施。

### 影响面

| 文件 | 调整 |
|---|---|
| `docs/00_design.md` §1.2 | **不改正文**（冻结）。多 bus 语义澄清在本条目登记；正式回写等第二个实测数据（XLeRobot v0.4 或 ATOM01 v0.5） |
| `docs/CHANGELOG.md` 本条 | 新增（**本次改动**） |
| `docs/targets/atom01.md` §5.1 | 风险表删除"非阻塞 fd 或 io_thread 代写"的错误建议；新增"每 bus 一个 rt_thread + per-bus seqlock slice"正解描述（**本次改动**） |
| `docs/targets/atom01.md` §4.1 | 表格 "S0 四线程" 行加脚注指向本条目（**本次改动**） |
| `docs/targets/xlerobot.md` §5.1 | feetech adapter 条目补"每 USB-RS485 一个 rt_thread"约定（**本次改动**） |
| `CLAUDE.md`（项目）"S0 线程模型（v0.2 骨架）" | v0.2 单 bus 骨架不动；加一行脚注指向本条目（**本次改动**） |
| `docs/01_implementation_steps.md` Step 6 | 不改（Step 6 仍 M=1 mock）；Step 10 Feetech 单关节也仍 M=1 |
| `crates/robot-transport::RtSnapshot*` API | v0.4 前不改；届时加 per-bus slice 维度 |
| `memory/rt_thread_per_bus_topology.md` | 新增 feedback memory（**本次改动**） |

### 审阅

- 提议：Claude Code（2026-04-24 session 内读完 atom01 上游 socket_can.cpp / dm_motor_driver.cpp / thread_pool.hpp / robot_interface.cpp 后对比用户 EtherCAT 经验整理）
- 决定：用户 loosqk（2026-04-24 session 内明确："就用方案b"）
- 候选回写 §1.2 的触发：
  - XLeRobot v0.4 真多 bus 落地 + cyclictest 1 h p99 < 1.5 ms、RT thread alloc = 0、`write()` 稳态 EAGAIN 计数 = 0 满足 → 正式回写 §1.2 为"每 HardRT bus 一实例"（走回写登记）
  - 如第一次真多 bus 实测发现 B 不达标（EAGAIN 频发 / 抖动超标）→ 评估回退到 atom01 式 TX queue 方案，**但 RX 仍不走 async callback**（保 §4.9 RT 铁律）
- 复审触发：
  - EtherCAT 真 slave 落地（≥ v0.6） → 验证本模式在 PDO 场景下依然成立（预期：drain_rx 变成"读 last cycle PDO"，接口一致）
  - Step 10 Feetech 单总线实测 `write()` 稳态延迟分布 → 写进 `docs/10_verification.md` 作 baseline

---

## 2026-04-24 · §4.7 ExternalPolicyHost 在 RT 内核下调度等价 inline + iceoryx2 RT baseline 提前到 Step 6（**状态：登记 / 验收条件冻结**）

### 涉及小节

- `docs/00_design.md` §4.7 "Policy runtime（InProcess vs External）"（不改正文；本条**收口 External 路径在 RT 内核下的调度风险**）
- `docs/01_implementation_steps.md` Step 6 退出标准（新增 iceoryx2 RT baseline）
- `docs/10_verification.md` §10.1.3 Transport（新增 iceoryx2 RT 行为指标）
- `docs/targets/atom01.md` §4.3 延迟预算 + §6 风险与门槛
- `docs/targets/xlerobot.md` §6 风险与门槛

### 背景

2026-04-24 session 里评估"`robot-system` 能否替代 `atom01_deploy`"时，唯一未 closed 的架构级风险是 **policy_host 独立进程 vs inline ONNX 推理的调度抖动** ——原栈 `inference_thread` SCHED_FIFO 70 inline，robot-system 走 iceoryx2 发/收 obs/action 到独立进程。

用户（loosqk）指出：**atom01 运行环境已经是 5.10 RT 内核（OPi 5+ 装 `assets/*.deb`）或 6.1.83 RT（RDK X5 出厂 RT）**，`rtprio 98 + memlock unlimited` 就位。在 RT 内核 + SCHED_FIFO + CPU pinning + mlockall 下，**独立进程的调度确定性与 inline 等价**，所谓"进程切换抖动"仅是未配 RT 调度时的默认 SCHED_OTHER 行为。

### 决定

1. **§4.7 External 路径在 atom01 部署环境（RT 内核）下不是架构风险**。具体配置：
   - `SCHED_FIFO prio = 70`（与原栈 `inference_thread` 同级）
   - 绑一个 A76 大核（RK3588 4 A76 + 4 A55，A76 ~2.4 GHz；若 A76 不够则让给 CAN rt_thread，policy_host 退 A55，实测取舍）
   - `isolcpus=` 内核 bootarg 剔掉 policy_host 所用核（避免 kernel 随意调度其他任务）
   - `mlockall(MCL_CURRENT | MCL_FUTURE)` 进程启动第一件事
   - `ort::intra_threads = 1` + 启动 2 次 warmup 丢弃（触发 MemPattern + JIT）

2. **XLeRobot（Jetson Orin 默认非 RT 内核）保留 §4.7 InProcess 分支作 fallback**。v0.4 立项时决定是否装 RT 内核；若不装，`robot-policy-host` 以 `feature = "inprocess"` 编进 `robot-s1`，丧失崩溃隔离换调度确定性。

3. **iceoryx2 pub/sub 的 RT 行为 baseline 提前到 Step 6 必产**。此前计划在 v0.5 ATOM01 落地时才测；user 明确"iceoryx2 要提前测"，改为 Step 6 退出标准硬门槛：
   - 场景：50 Hz publish、10 min、Linux RT 内核 + `SCHED_FIFO 70` + CPU pin
   - 指标：receive 时刻相对 publish 的 delta，p99 < 100 µs，p999 < 500 µs
   - 实测环境：`linux-x86_64-nightly-YYYYMMDD` env_id 登记到 `docs/10_verification.md` 附录

### 验收清单（三条）

与 atom01 替代 policy_host 部分相关的验收项：

| 验收项 | 目标 | 落地 Step | 状态 |
|---|---|---|---|
| iceoryx2 RT 行为 baseline | p99 < 100 µs、p999 < 500 µs（50 Hz / 10 min / RT 内核 + SCHED_FIFO）| **Step 6** | ⏳ |
| ORT 稳态 0 alloc（warmup 后）| 第 3 次及之后 `Run()` 经 `assert-no-alloc` 不触发 | v0.5 `robot-policy-host` 落地 | ⏳ |
| 双 shm round-trip (obs + action) | p99 < 50 µs | v0.5 policy_host 接入 | ⏳ |

三条全过 = `§4.7` External 路径在 atom01 部署环境收口；任一不过 → 切回 `InProcessPolicyHost`（`robot-s1` feature flag），设计 §4.7 的 fallback 路径已冻结，无需再评审。

### 理由

- **RT 内核在手**：atom01 硬件环境既定前提，不是额外工程成本。原栈已经依赖 `rtprio` / `memlock`，robot-system 不多付出。
- **inline 和 External 的调度差距在 RT 内核下 = iceoryx2 shm 双向 round-trip 的延迟**：预期 p99 < 50 µs，在 20 ms 推理周期（50 Hz）上是 < 0.25% 预算。
- **ORT 自身是 CPU-bound 计算**（~5-15 ms Run），整个推理期间 policy_host 线程独占核，正好是 SCHED_FIFO 的设计意图；非 RT 内核下才会被抢占。
- **iceoryx2 RT baseline 提前到 Step 6** 的价值：Step 6 是第一个 Linux RT 场景，一起测 cyclictest + iceoryx2 roundtrip 时间机器; 拖到 v0.5 才发现 iceoryx2 本身不够 RT，返工成本高。

### 约束

1. **iceoryx2 RT baseline 不过 → block Step 6 退出**。若 p99 / p999 达不到，不继续 Step 7–10，先调查原因（iceoryx2 版本 / kernel tunable / 测试方法）。
2. **Step 6 iceoryx2 RT 测试必须配 SCHED_FIFO + CPU pin + mlockall**，不是"macOS 开发机随便跑"—— 设备必须是 Linux RT 内核（用户 Linux 开发机 / Orin / Orange Pi 5+ 其中之一）。
3. **RT 内核 tunable 清单**（写入 `scripts/collect_env_id.sh` 采集项）：
   - `/proc/cmdline`（`isolcpus`、`rcu_nocbs`、`threadirqs`、`idle=poll`）
   - `/sys/devices/system/cpu/cpu*/cpufreq/scaling_governor`（建议 performance）
   - `/proc/sys/kernel/sched_rt_runtime_us`（不能限制 RT 抢占）
4. **XLeRobot 不强制 RT 内核**：v0.4 立项时实测 Orin 默认 kernel 下 InProcess 和 External 两种模式延迟差，决定是否装 RT。

### 影响面

| 文件 | 调整 |
|---|---|
| `docs/01_implementation_steps.md` Step 6 退出标准 | 新增 "iceoryx2 RT behavior baseline 落 `docs/10_verification.md`"（**本次改动**） |
| `docs/10_verification.md` §10.1.3 | 新增 "iceoryx2 publish→receive 延迟 p99 < 100 µs / p999 < 500 µs（RT 内核 + SCHED_FIFO 70）" 指标行，状态 ⏳ Step 6（**本次改动**） |
| `docs/targets/atom01.md` §4.3 | 延迟预算段落改写：显式说 RT 内核在手，policy_host 调度与 inline 等价（**本次改动**） |
| `docs/targets/atom01.md` §6 | policy_host 抖动 + SocketCAN write() 两条从风险降级到验收项（**本次改动**） |
| `docs/targets/xlerobot.md` §6 | 新增 "policy_host 调度抖动（Orin 默认非 RT 内核）" 风险 + v0.4 取舍说明（**本次改动**） |
| `docs/CHANGELOG.md` 本条 | 新增（**本次改动**） |
| `docs/00_design.md` §4.7 | **不改正文**（冻结，InProcess / External 两路本就都在）；本条收口 External 在 atom01 场景的判断 |

### 审阅

- 提议：Claude Code（2026-04-24 session 内"RT 内核可用 → policy_host 调度抖动不是真风险"整合）
- 决定：用户 loosqk（2026-04-24 session 内明确："policy_host 可以用绑定内核加高优先级测试。因为 atom01 运行环境已经是 RT kernel" + "iceoryx2 要提前测，都落进去"）
- 复审触发：
  - Step 6 iceoryx2 RT baseline 测出来 p99 超标 → 评估换 iceoryx2 版本 / 或切 InProcess 作 atom01 默认
  - v0.5 ATOM01 `robot-policy-host` 落地实测若 ORT warmup 后仍有 alloc → 加 preallocated arena 或切 InProcess
  - XLeRobot v0.4 实测 Orin 默认 kernel 下 External 能用 → 回撤 InProcess fallback 的必要性；装 RT 内核变成可选

---

## 2026-04-24 · Step 6.5 RT 调优搁置 + XLeRobot 不卡 §10.1 RT 硬门槛（**状态：偏离登记**）

### 涉及小节

- `docs/00_design.md` §10.1 "S0 loop / Safety / crash-restart 指标"（**不动**；本条只把 XLeRobot target 的适用性收口，§10.1 数值对 ATOM01 / 人形 / 四足 仍是契约）
- `docs/01_implementation_steps.md` 顶部"当前位置"行（同步标 Step 6.5 暂不做的理由）
- `docs/targets/xlerobot.md` §1 实时内核行 + §6 风险表（已注 "Orin 默认即可"，本条把 Step 6.5 搁置理由补足）
- `.github/workflows/ci.yml`（**本次改动**：仅挂宽阈值 ubuntu smoke，不挂 Orin self-hosted strict-rt job）
- `scripts/baseline.sh`（**本次改动**：新建封装，本机 / Orin 两模式，门槛不严卡）

### 背景

`docs/handoffs/2026-04-24_5.md` §5 task 22 实测 Orin Nano Super 三轮 60s/30s strict-rt：

```
配置                  | p50    | p99       | p999      | max
--duration 60s       | 22.5µs | 1018.4µs  | 19955.7µs | 20021.2µs   FAIL
--duration 60s --cpu4| 20.5µs | 5746.7µs  | 6447.1µs  | 6504.4µs    FAIL
--duration 30s --cpu4| 21.0µs | 15237.1µs | 15491.1µs | 15499.3µs   FAIL
```

p50 21 µs 表稳态 OK，p99 1–15 ms 是 PREEMPT 内核 + schedutil governor + 无 isolcpus 的系统瓶颈（详 handoff §5.2）。原计划走 handoff §5.3 的 sudo 调优清单（governor / cpu_dma_latency / isolcpus / PREEMPT_RT 内核）→ Step 6.5 闯门槛。

用户（loosqk）2026-04-24 明确两条搁置 Step 6.5 的理由：

1. **XLeRobot 不需要高实时性**：Feetech STS3215 自带 command timeout，硬件层 fail-safe；上层 50 Hz / 20 ms 周期对 100 µs / 500 µs 没有刚需。原 §10.1 硬门槛是按 ATOM01 5×CAN 多 bus 同步刚需推导的（2026-04-24 多 bus 拓扑方案 B 条），XLeRobot 单 USB-RS485 不是同一量级。
2. **Orin Nano 算力本身偏弱**：6 核 A78AE，A78AE 相对 RK3588 A76 缺 RT 内核厂家支持（JetPack 默认 PREEMPT 不是 PREEMPT_RT），自编 PREEMPT_RT 内核 + driver 重新调通的工程成本，对一个不需要硬实时的目标不划算。

### 决定

1. **Step 6.5（Orin RT 调优攻克 §10.1 硬门槛）暂不做**。`docs/01_implementation_steps.md` 10 步主线不受影响（Step 6 退出标准里"iceoryx2 RT baseline harness 就位 + 跑通"已在 task 21 落地，门槛达标 *不是* Step 6 退出条件 —— 见 handoff §1）。
2. **`docs/00_design.md` §10.1 数值不动**：对 **ATOM01 / 四足 / 人形** 等目标仍是契约（这些 target 跑 RT 内核 + 多 bus，门槛是真刚需）。XLeRobot 是 §10.1 数值的 *target-specific 不适用*，不是数值本身的修订。
3. **CI 不挂 Orin self-hosted runner 的 strict-rt baseline job**：理由同 (1)，挂上去会因系统瓶颈持续红 CI；ubuntu-latest 30s smoke job 仍要（本次新增），用宽阈值 p99 ≤ 50 ms / p999 ≤ 100 ms，作用是防 `iceoryx2_rt_baseline` harness 代码退化。
4. **`scripts/baseline.sh`**（本次新增）封装 handoff §5.4 的两条命令，Orin 真要刷数据时手动跑：
   - `./scripts/baseline.sh local [DUR]` —— 本机 release smoke
   - `./scripts/baseline.sh orin  [DUR]` —— ssh Orin 跑 strict-rt（默认 60s + `--cpu 4`）
5. **XLeRobot target 走 §4.7 InProcess fallback** 已在 2026-04-24 "§4.7 ExternalPolicyHost RT 等价 inline" 条 §决定 (2) 锁定，本条只追加注脚：v0.4 立项时的"是否装 RT 内核"决定可以 *直接拍板不装*，理由是 (1)+(2)。

### 约束

1. **本条只对 XLeRobot 生效**。其他 target（ATOM01 / 四足 / 人形）开新 spec 时，`§5/§6` 风险表必须显式声明 "适用 §10.1 RT 硬门槛"，否则视同同一豁免。
2. **`docs/00_design.md` §10.1 文本不能加 "XLeRobot 例外"**（设计冻结）；解释一律放在本 CHANGELOG 条 + `targets/xlerobot.md` §1+§6。
3. **复审触发**：
   - XLeRobot 加第二条 USB-RS485（双臂 / 移动底盘 + 头部）→ Feetech bus 数 ≥ 2 后重新评估是否进入 §10.1 门槛域；
   - dimos 现役栈实测出 ≥ 30 ms 控制延迟 spike 导致行为退化 → 评估是否要装 RT 内核 + cpu_dma_latency；
   - ATOM01 v0.5 落地，发现 §10.1 数值需要进一步细化（per-bus / per-target 数值表）→ 顺带把 XLeRobot 行写进去。
4. **scripts/baseline.sh 的宽阈值不能视作"达标线"**。它是 *harness 不退化* 的最低线；真要拿数据走严阈值需手动传 `--p99-us 100 --p999-us 500`（即 §10.1 数值）。

### 影响面

| 文件 | 调整 |
|---|---|
| `.github/workflows/ci.yml` | 新增 `baseline-smoke` job（ubuntu，30s，宽阈值，依赖 `test`）（**本次改动**） |
| `scripts/baseline.sh` | 新增（**本次改动**） |
| `docs/01_implementation_steps.md` | 顶部"当前位置"行同步：Step 6.5 不做 + 理由指针（**本次改动**） |
| `docs/targets/xlerobot.md` | §1 实时内核行 + §6 风险表注脚补 "Step 6.5 搁置 → 走 InProcess fallback"（**本次改动**） |
| `docs/CHANGELOG.md` 本条 | 新增（**本次改动**） |
| `docs/00_design.md` §10.1 | **不动**（设计冻结） |
| `docs/handoffs/2026-04-24_5.md` | **不动**（handoff 本身就是历史快照，不回写） |

### 审阅

- 提议：Claude Code（2026-04-24 session 内 Step 7 task 24 入手时）
- 决定：用户 loosqk（2026-04-24 session 内明确："推迟 6.5 的原因是 1. 他跑的 xlerobot 不需要高实时性。2. orin nano 本身性能就弱"）
- 复审触发：见上方"约束 §3"

---

## 2026-04-24 · Step 7 task 25 robot-s0 / robot-core::config 依赖白名单更新 + ConfigIdentity v0.2 部分骨架（**状态：登记 / 部分实现**）

### 涉及小节

- `docs/00_design.md` §7.2（TOML schema）+ §7.3（`ConfigIdentity`）—— **不动**正文
- `docs/CHANGELOG.md` 2026-04-22 "workspace runtime 依赖白名单机制" 各 crate 子清单（本条更新 `robot-s0` / 新增 `robot-core::config` 例外）
- `crates/robot-core/src/config.rs`（**本次新增**：`IdentityFingerprint` 类型 + 4 单测）
- `crates/robot-s0/src/config_loader.rs`（**本次新增**：TOML schema strut + 加载入口 + 9 单测）
- `crates/robot-s0/src/bin/s0_daemon.rs`（**本次改动**：`Args` → `ResolvedArgs` 拆分 + `--config` 参数 + identity 打印）
- `configs/xlerobot.toml`（**本次新增**：v0.2 示例）
- `Cargo.toml` workspace.dependencies 加 `serde` / `toml = "0.9"`

### 背景

设计 §7.3 `ConfigIdentity` 是跨进程启动 handshake 的核心：5 个 hash + `robot_id` + `config_version`，用于把"S0 升级后 joint 顺序变了，S1 还用旧配置"这类隐性事故拦在启动期。

`docs/handoffs/2026-04-24_5.md` §8 task 25 入口清单要求：v0.2 阶段 **替代纯 CLI 模式**，从 `configs/<robot>.toml` 读取配置 + 算 `ConfigIdentity`。但 §7.3 描述的"跨进程 iceoryx2 request-response 互换 + 不一致进 `DEGRADED`" 完整实现需要：calibration / schema_registry / adapter inventory **三个 v0.3+ 子系统**全部就位才能算齐 4 hash。强行 v0.2 全做会扯 v0.3 才该做的事。

### 决定

1. **拆分 v0.2 partial vs v0.3 full**：
   - v0.2 只算 `config_hash`（blake3 over TOML 原始 bytes）+ `robot_id` + `config_version`；
   - 余 `calibration_hash` / `schema_registry_hash` / `adapter_registry_hash` 留 `[0u8; 32]` placeholder；
   - 类型 `IdentityFingerprint::v02_partial(...)` + `is_fully_partial() -> bool` 把"v0.2 占位"语义显式化，避免 v0.3 误用 placeholder 做真比对。
   - Display 在 placeholder 字段后缀 `*`，人眼一秒分辨"v0.2 vs v0.3"。
2. **跨进程 handshake 推 v0.3+**：本任务**不**改 state machine、**不**接 iceoryx2 request-response、**不**实现 `DEGRADED` 分支。s0_daemon 仅启动期打印一次 identity，仅供观测。
3. **类型分层 + 依赖隔离**：
   - 纯类型 `IdentityFingerprint` → `robot_core::config`，**无** serde derive，恪守 §4.2 "零 runtime 依赖"。
   - TOML 解析逻辑（含 `serde` derive + `toml::from_str`）→ `robot_s0::config_loader`。`robot-s0` 是 Linux binary crate，加 `serde` / `toml` / `blake3` / `thiserror` 不污染跨平台核心。
   - v0.3 起若 `robot-s1` / `robot-recorder` 也要读同一份 TOML，再抽 `robot-config` 共享 crate（v0.2 单 binary 不值得抽）。
4. **CLI > TOML > env > 默认 优先级**：
   - `s0_daemon --robot-id` 仍可 override TOML `[robot] id`，便于多实例调试 / 同 robot 多硬件 (`--hardware-id` 同理)。
   - **identity 始终来自 TOML**（不跟 CLI override 走）：v0.2 阶段允许这一不对称，因为没有跨进程 handshake 真消费 identity；v0.3 跨进程 handshake 落地时若发现 CLI override 导致 identity 漂移引发问题，再决策（候选：CLI override 时强制重算 identity，或禁止 CLI override 已有 `--config` 字段）。
5. **TOML schema 字段宽松**：`[[hardware]]` / `[[sensor]]` 仅 `id` + `adapter` 必填，其他字段（`baud` / `wheel_radius` / `joints` / `streams` / ...）按需出现，未知字段不报错，由 `extras: toml::Table` flatten 兜底。v0.4 真 adapter 落地时按 adapter 名做 enum dispatch；v0.2 不锁 schema。

### 各 crate 白名单更新

#### `robot-core`（§4.2 + 2026-04-21 libc 例外，本次**不变**）

- `robot-core::config` 模块**只**用 std + 自身已有依赖（无 `serde` / `toml` / `blake3`）。
- 新增类型 `IdentityFingerprint` 是纯 POD（`String` + `[u8; 32]`），不破坏"零 runtime 依赖"。

#### `robot-s0`（v0.2 Step 6 → Step 7 task 25 扩展）

| 依赖 | 范围 | 用途 | RT 影响 |
|---|---|---|---|
| `serde = { workspace = true, features = ["derive"] }` | **本次新增** | `config_loader::RobotConfigToml` deserialize | 仅 daemon 启动期 parse；RT 线程未 link |
| `toml = { workspace = true }`（v0.9） | **本次新增** | TOML 解析 | 同上；与 iceoryx2 0.8.1 子树同 0.9.x，避免 dup-version warning |
| `blake3 = { workspace = true }` | **本次新增** | `compute_v02_partial_identity` | 启动期一次性 hash；RT 线程未调用 |
| `thiserror = { workspace = true }` | **本次新增** | `ConfigLoadError` derive | 同已有 `robot-core::error::RobotError` |

#### workspace.dependencies（首次声明）

```toml
serde = { version = "1", features = ["derive"] }
toml  = "0.9"
```

### 约束

1. **不改 §4.2 robot-core 白名单文字**：本条只在 `robot-s0` 范围引入 serde / toml；`robot-core::config` 仍纯类型。后续若 `robot-msg` / `robot-transport` 等其他核心 crate 想加 serde，单独走 CHANGELOG 评审，不视同本条豁免。
2. **`config_hash` 用 raw bytes 非 canonical 化**：调用方注意 line-ending / trailing newline 差会改 hash。v0.3 跨进程 handshake 实测出"开发机 vs 部署机的 newline 差成痛点"再升 canonical 化。
3. **identity 不参与 v0.2 任何决策**：仅打印一次，禁止任何 v0.2 路径分支（state machine / RPC / topics）依赖 identity 字段值；v0.3 接 handshake 后再开放消费。
4. **`extras: toml::Table` 字段不进类型契约**：v0.4 真 adapter 落地时按 adapter 名 enum dispatch + 强类型 schema；v0.2 的 `extras` 仅作"不报错"占位，不做契约检查。

### 影响面

| 文件 | 调整 |
|---|---|
| `Cargo.toml`（workspace） | `[workspace.dependencies]` 加 `serde` / `toml` |
| `crates/robot-core/Cargo.toml` | **不动**（无新增依赖） |
| `crates/robot-core/src/lib.rs` | `pub mod config` |
| `crates/robot-core/src/config.rs` | 新增 |
| `crates/robot-s0/Cargo.toml` | dependencies 加 `serde` / `toml` / `blake3` / `thiserror` |
| `crates/robot-s0/src/lib.rs` | `pub mod config_loader` |
| `crates/robot-s0/src/config_loader.rs` | 新增 |
| `crates/robot-s0/src/bin/s0_daemon.rs` | `Args` → `ResolvedArgs` + `--config` + identity 打印 |
| `configs/xlerobot.toml` | 新增（v0.2 示例：robot + 3 hardware mock + 1 sensor） |
| `docs/CHANGELOG.md` 本条 | 新增 |
| `docs/00_design.md` §7.2 / §7.3 | **不动**（设计冻结） |

### 审阅

- 提议：Claude Code（2026-04-24 session 内 Step 7 task 25 入手时）
- 决定：用户 loosqk（2026-04-24 session 起手 prompt 已锁定 task 顺序：CI baseline → ConfigIdentity → rt_guard → feetech → 真机）
- 复审触发：
  - v0.3 跨进程 handshake 落地，发现 CLI override identity 漂移成问题 → 评估"identity 跟 CLI 走"还是"CLI override 禁止"（决定 §4 留的口子）。
  - v0.4 真 adapter 落地，发现 `extras: toml::Table` 太宽（adapter 字段拼写错被静默 ignore）→ 评估按 adapter 名 enum dispatch 的实现节奏。
  - 第二个 binary（robot-s1 / robot-recorder）需要读同一 TOML → 评估抽 `robot-config` 共享 crate（决定 §3 留的扩展点）。

---

## 2026-04-24 · Step 7 task 27 robot-adapter-feetech 协议层 + adapter trait 实现（**状态：登记 / 部分实现**）

### 涉及小节

- `docs/CHANGELOG.md` 2026-04-22 "workspace runtime 依赖白名单机制" → 新增
  `robot-adapter-feetech` 子条目
- `docs/01_implementation_steps.md` Step 10 "Feetech 单关节" → task 27 完成
  *协议层 + adapter trait 实现*；剩 *真硬件 wire 路径 + IO 线程* 留 task 28
- `Cargo.toml` workspace.members 新增 `crates/robot-adapters/feetech`
- 不动 `docs/00_design.md` §4.4 / §4.6（trait 形状不变）
- 不动 `docs/00_design.md` §10.1（feetech adapter 自身不引入新 RT 指标）

### 背景

`docs/handoffs/2026-04-24_5.md` §8 task 27 入口清单：把 dimos `feetech_io/`
（8 文件 / ~900 行 Python）+ `06_lerobot_replace_spec.md` 移植成 Rust
adapter，挂 `ManipulatorAdapter` trait。

dimos 端走 `scservo_sdk` Python 绑定；Rust 端**不**链接 `scservo_sdk`，wire
protocol 自实现（packet 头 + checksum + ping/read/write/sync_*）。这样：
- 无 Python / vendor SDK 依赖，CI 可全量编译；
- 协议层每帧字节级单测可比对；
- task 28 真机阶段只换 `SerialTransport` 实现，protocol / bus / adapter 层
  零改动。

### 决定

1. **新 crate `robot-adapter-feetech`**（`crates/robot-adapters/feetech/`）：
   - `registers` —— 14 项控制表 + sign-magnitude 表 + EEPROM 写保护标记 +
     `OperatingMode` / `TorqueMode` enum，字面对照 dimos `registers.py` 117 行。
   - `encoding` —— sign-magnitude encode/decode + LE byte split/combine，对照
     `06_lerobot_replace_spec.md` §1.4 / §2 + dimos `encoding.py`。
   - `protocol` —— `build_ping` / `build_read` / `build_write` / `build_sync_*`
     / `parse_status` / `find_header`，字节级单测对照 Dynamixel protocol 1
     标准（`PING ID=1` = `FF FF 01 02 01 FB`，`WRITE Goal_Position=2048` =
     `FF FF 01 05 03 2A 00 08 C4`）。
   - `transport` —— `SerialTransport` trait + 内存 `MockTransport`（仅单测）。
   - `bus` —— `FeetechBus` 公开 API：`new` / `connect` / `ping` / `read` /
     `write` / `sync_read` / `sync_write`，sign-magnitude 编解码透明，调用方
     一律传/收 `i32`。
   - `calibration` —— `unlock_eeprom` / `lock_eeprom` / `reset_calibration` /
     `write_calibration` / `set_half_turn_homings`，字面移植 dimos
     `calibration.py` 算法。EEPROM `Lock=0` + `Torque_Enable=0` 显式写。
   - `adapter` —— `FeetechAdapter` 实现 `ManipulatorAdapter` trait（9 方法）。
     单位换算：12-bit tick (4096/圈) ↔ rad，中点 = 2047 = 0 rad。
   - `error` —— `FeetechError` 富信息 enum，`From<FeetechError> for RobotError`
     映射成 `&'static str` 静态消息（恪守 robot-core::error 零堆分配约束，
     富信息走 `eprintln!` 在 adapter 层落地）。
   - 83 单元测试，全部用 `MockTransport` 喂虚假回包 → 字节级验证 wire 正确性。

2. **同步阻塞模型 + 文档警告**：v0.2 `read_state` / `write_command` 在调用线程
   内做完整 wire RTT（ms 量级，违反 §4.9 RT 铁律 < 100 µs）。`info().rt_class`
   仍标 `HardRT` —— "目标分类"，不代表 v0.2 已就绪挂 `s0_rt_thread`。task 28
   实测后决定是否引入"adapter 内部 IO 线程 + atomic snapshot"双层。

3. **不引入 `serialport` crate**（task 27 范围）：真 USB-RS485 实现走 task 28
   的 `feature = "real-hw"`，本任务零硬件依赖，CI 可在 mac / Linux runner 全
   量跑通。

4. **不接 inventory + matrix**：留 task 27b（matrix + 自注册），task 27 仅
   交付独立 crate；s0_daemon 当前继续硬 link `robot-adapter-mock`。

### `robot-adapter-feetech` 依赖白名单

| 依赖 | 范围 | 用途 |
|---|---|---|
| `robot-core = { workspace = true }` | dependencies | trait `ManipulatorAdapter` + POD 类型 + `RobotError` |
| `thiserror = { workspace = true }` | dependencies | `FeetechError` derive Display/Error |
| 无 dev-dep | | mock transport 在 src 内自实现，不引外部 |

**预留**（task 28 加）：
- `serialport`（feature `real-hw`）—— USB-RS485 真后端

**禁止**：
- 任何 vendor C/C++ SDK（含 `scservo_sdk`）—— wire protocol 自实现
- `tokio` —— v0.2 同步路径，无 async runtime；task 28 IO 线程方案待定
- `serde` —— calibration JSON 等 v0.4+ 走 `robot-core::config` 共享路径

### 约束

1. **`info().rt_class = HardRT` 是目标分类，不是当前能力**。挂 `s0_rt_thread`
   在 task 28 IO 线程模型落地前**禁止**；当前只能用作离线校准 / 单元测试 /
   非 RT 控制路径（如 robot-cli debug）。
2. **PING 的 model number 通过显式第二步 READ_DATA(addr=3, len=2) 拿到**，
   而非依赖 scservo_sdk 的隐式 PING+READ 复合行为。dimos / lerobot 移植对照时
   注意**两步**对应一次 `bus.ping()` 调用。
3. **EEPROM 写保护必须显式做**：`unlock_eeprom`（`Torque_Enable=0` +
   `Lock=0`）→ 写 EEPROM 寄存器 → `lock_eeprom`（`Lock=1`，**不**重新打开
   `Torque_Enable`）。漏 `Lock=0` → motor 静默忽略写（`06_lerobot_replace_spec.md`
   §11 风险 1）。
4. **rad ↔ raw 单位假设全圈 2π / 4096 tick**：v0.2 不接 calibration / drive_mode
   反转 / 限位裁剪。task 28 真机阶段实测 → 视情况引入 calibration JSON。
5. **velocity / torque raw 单位 v0.2 直接透传 f32**（lerobot 也未做精确换算）。
   task 28 用示波器 + known 命令对照定单位转换。

### 影响面

| 文件 | 调整 |
|---|---|
| `Cargo.toml`（workspace） | members 新增 `crates/robot-adapters/feetech`；workspace.dependencies 加 `robot-adapter-feetech` path |
| `crates/robot-adapters/feetech/` | **新增**整 crate（lib + 9 模块 + 83 单测） |
| `docs/CHANGELOG.md` 本条 | 新增 |
| `docs/00_design.md` | **不动**（trait 形状不变） |
| `docs/01_implementation_steps.md` Step 10 | 后续 task 28 commit 同步更新 "当前位置" |

### 审阅

- 提议：Claude Code（2026-04-24 session 内 Step 7 task 27 入手时）
- 决定：用户 loosqk（2026-04-24 session 内"上真机前冒烟测试 / 全量测试 /
  性能测试 / 火焰图都要搞好" → 拆出 task 27c 阻 task 28；本条只负责协议层 +
  adapter trait 实现）
- 复审触发：
  - task 28 真机实测发现 wire protocol 字节级与 scservo_sdk 不一致 → 回头改 protocol.rs
  - task 28 实测同步 `read_state` 单 tick 延迟 > 5 ms → 引入 adapter 内部 IO 线程 + atomic snapshot 双层架构
  - 第二个 motor 型号需要支持（SMS / SCS 系列）→ registers / sign_bit / model_number 表抽 trait

---

## 2026-04-24 · Step 7 task 27b matrix 编译 + inventory adapter 自注册 + CI 矩阵（**状态：登记 / 实施细节落地**）

### 涉及小节

- `docs/00_design.md` §4.5 "静态注册"（**不动正文**；本条把 §4.5 描述的
  inventory + factory 模式具体实现落地）
- `docs/CHANGELOG.md` 2026-04-22 "workspace runtime 依赖白名单机制" → 新增
  `inventory` 进 `robot-core` / `robot-adapter-mock` / `robot-s0` 白名单条目
- `Cargo.toml` workspace.dependencies 加 `inventory = "0.3"`
- `crates/robot-core/src/factory.rs`（**本次新增**：AdapterBuildContext + AdapterFactory + find_factory + registered_factories + 4 单测）
- `crates/robot-core/src/adapter.rs` 加 `Box<Inner>: ManipulatorAdapter` blanket impl
- `crates/robot-adapters/mock/src/lib.rs` 加 `MockFactory` + `inventory::submit!`
- `crates/robot-s0/Cargo.toml` adapter dep 改 optional + 新增 features `mock-only` / `xlerobot` / `atom01`（基础 `mock` / `feetech`）
- `crates/robot-s0/src/lib.rs` `pub use ... as _` 强制 link（防 dead-code strip）
- `crates/robot-s0/src/bin/s0_daemon.rs` 替换硬 link `MockHardRTAdapter` 为 `find_factory(...).build(ctx)`
- `.github/workflows/ci.yml` 新增 `matrix-build` job（mock-only / xlerobot / atom01 三 combo build + test）

### 背景

`docs/00_design.md` §4.5 + `CLAUDE.md` "robot-kinematics-<robot> 按 feature flag
挂到 robot-s1" 模式：每种机器人配置 link 不同 adapter 子集，s0_daemon 启动期
按 `configs/<robot>.toml` `[[hardware]] adapter = "..."` 字符串实例化对应
factory。

task 24-27 期间 s0_daemon 一直硬 link `MockHardRTAdapter`，没接 inventory 也没
matrix。task 27 落 feetech 协议层后切要解决"不同 robot link 不同 adapter"问题，
否则 task 28 真机 + 后续 piper / realsense 接入时还是改 s0_daemon 硬 link 的
反模式。

### 决定

1. **`robot-core::factory` 模块**：
   - `AdapterBuildContext<'_>` —— v0.2 最小集字段（`instance_name` / `adapter_kind`
     / `joint_names`），不含 `address` / `baud` 等 adapter-specific 字段
     （v0.4 真硬件 adapter 落地时按需扩；扩字段保证不破坏现有 factory 兼容）。
   - `AdapterFactory: Sync + 'static` trait —— `name()` + `build(&ctx) ->
     Result<Box<dyn ManipulatorAdapter + Send>, RobotError>`。
   - `inventory::collect!(&'static dyn AdapterFactory)` + `find_factory(name) ->
     Option<&'static dyn AdapterFactory>` + `registered_factories() -> impl Iterator`。
2. **`Box<Inner>: ManipulatorAdapter` blanket impl** 加在 `robot-core::adapter`
   末尾 —— 让 `Box<dyn ManipulatorAdapter + Send>` 直接满足 `spawn_rt_thread<A>`
   的泛型约束。零 runtime 开销（vtable 间接 + Box deref，与显式 dyn 调用等价）。
3. **每个 adapter crate 自注册**：mock 已注册；feetech 暂不注册（缺 SerialTransport
   选择 + 真硬件依赖，留 task 28 加 `real-hw` feature 时再注册）。
4. **`robot-s0` features 设计**：
   ```toml
   default   = ["xlerobot"]            # 开发期默认拿到 mock + feetech
   mock-only = ["mock"]
   xlerobot  = ["mock", "feetech"]
   atom01    = ["mock"]                # piper crate 待 v0.5 落地后扩 ["mock", "piper"]
   mock      = ["dep:robot-adapter-mock"]
   feetech   = ["dep:robot-adapter-feetech"]
   ```
   组合 features (`xlerobot` / `atom01` / `mock-only`) 对外暴露给用户 + CI；
   基础 features (`mock` / `feetech`) 内部 gating 单 crate dep。
5. **强制 link 防 dead-code strip**：`robot-s0/src/lib.rs` 加
   `#[cfg(feature = "X")] pub use robot_adapter_X as _;`。Rust 不会 strip
   被 `pub use` 的 lib，inventory section 被合并入最终 binary。
6. **s0_daemon adapter 来源**：
   - 有 `--config`：取 TOML 第一段 `[[hardware]]` 的 `adapter` + `joints` 字段，
     走 `find_factory` 查 + `factory.build(&ctx)`。
   - 无 `--config`：fallback 到 `mock_hard_rt`，joint 名按 `j0..jN-1` 自动生成
     （兼容旧 CLI smoke 路径）。
   - 找不到 factory → 打印"已注册：[...]"清单 + exit 2，便于排查 feature 漏开 / 拼写错。
7. **CI matrix-build job** 跑 3 combo（mock-only / xlerobot / atom01），每个
   `cargo build --release -p robot-s0 --bin s0_daemon --no-default-features
   --features <combo>` + `cargo test`。新机器人 entry 加一行 matrix 即可。

### `inventory` 依赖白名单

新增 `inventory = "0.3"` 到 workspace.dependencies，加进 `robot-core` /
`robot-adapter-mock` / `robot-s0` 白名单。

理由：链接器把 `submit!` 项收到 array section，**零 runtime 开销**（无 alloc /
无 IO，仅启动期一次性遍历）。技术上不破坏 §4.2 "robot-core 零 runtime 依赖" 精神，
但形式上是新依赖 → 走白名单登记。

### 约束

1. **`AdapterFactory::name()` 必须与 `[[hardware]] adapter = "..."` 字符串字面
   一致**。本次约定 snake_case：`mock_hard_rt` / `feetech_sts3215` / `piper_*`
   / `realsense_*`。新 factory 在 module-level docs 显式标注与 TOML 字符串关系。
2. **`AdapterBuildContext` 字段扩展不破坏旧 factory**：v0.4 加 `address` /
   `baud` 等字段时，旧 factory 忽略不读即可，新 factory 读新字段。
3. **`info().rt_class = HardRT` 是声明而非编译期保证**：trait 无法静态保证
   `read_state` / `write_command` 不 IO 阻塞。task 27 feetech 同步实现 + task 27c
   bench 报告 + task 28 真机实测 一起把"声明 vs 实测"对齐。
4. **inventory 链接行为依赖 binary link**：lib crate 内 `submit!` 必须经过
   `pub use ... as _` 或类似机制让 binary 真 link。否则 dead-code strip 会丢失
   注册项（test build 因 cargo 默认 link 全 lib，不受影响；release 优化下必中招）。

### 影响面

| 文件 | 调整 |
|---|---|
| `Cargo.toml`（workspace） | `[workspace.dependencies]` 加 `inventory = "0.3"` |
| `crates/robot-core/Cargo.toml` | `[dependencies]` 加 `inventory` |
| `crates/robot-core/src/lib.rs` | `pub mod factory` |
| `crates/robot-core/src/factory.rs` | **新增**（4 单测：自注册 stub + find / iter / context） |
| `crates/robot-core/src/adapter.rs` | 加 `Box<Inner>: ManipulatorAdapter` blanket impl |
| `crates/robot-adapters/mock/Cargo.toml` | `[dependencies]` 加 `inventory` |
| `crates/robot-adapters/mock/src/lib.rs` | `info().name` 改 `mock_hard_rt` 与 factory 一致；末尾加 `MockFactory` + `inventory::submit!` |
| `crates/robot-s0/Cargo.toml` | adapter dep 改 optional；features 新增 `mock-only` / `xlerobot` / `atom01` / `mock` / `feetech` |
| `crates/robot-s0/src/lib.rs` | `pub use robot_adapter_X as _` 强制 link |
| `crates/robot-s0/src/bin/s0_daemon.rs` | 移除 `use robot_adapter_mock::MockHardRTAdapter`；新增 `build_adapter` helper 走 inventory |
| `.github/workflows/ci.yml` | 新增 `matrix-build` job（3 combo） |
| `docs/CHANGELOG.md` 本条 | 新增 |
| `docs/00_design.md` §4.5 | **不动**（设计冻结） |

### 审阅

- 提议：Claude Code（2026-04-24 session 内 task 27b 入手时）
- 决定：用户 loosqk（2026-04-24 session 内"能不能做成矩阵式加载或者 ci，
  不同的机器人编译不同的模块" → 拍板方案 A：先 task 27 协议层 → task 27b
  matrix → task 27c bench → task 28 真机）
- 复审触发：
  - task 28 加 feetech factory + `real-hw` feature → 评估 feetech 注册 factory 时
    SerialTransport 来源（factory 内部硬编码 `serialport::new(addr)`，还是 context
    里加 `address` 字段后 factory 读？）
  - 第三个 robot（piper / 四足）落地 → 重新审视 features 命名约定（是否要
    `xlerobot-arms` / `xlerobot-base` 之类的二级拆分）
  - inventory crate 升级到 0.4 / `linkme` 替代 → 评估 API 兼容性

---

## 2026-04-24 · Step 7 task 27c feetech 性能 / 集成 smoke / 火焰图配套（**状态：登记 / 实施细节落地**）

### 涉及小节

- `docs/10_verification.md` §10.2.1 Harness 清单 → 新增 3 条 task 27c 产出
- `docs/10_verification.md` 附录 → 新增 feetech bench baseline 实测数值
- `Cargo.toml` workspace.dependencies 加 `criterion = "0.5"`（仅 dev-deps 用）
- `crates/robot-adapters/feetech/Cargo.toml` 加 `criterion` dev-dep + `[[bench]]` entry
- `crates/robot-adapters/feetech/benches/feetech_bench.rs`（**本次新增**）
- `crates/robot-adapters/feetech/tests/integration_smoke.rs`（**本次新增**）
- `scripts/feetech_flamegraph.sh`（**本次新增**）

### 背景

用户拍板"上真机前，冒烟测试，全量测试，性能测试和火焰图都要搞好"。task 27
完成 feetech 协议层 + 83 单元测试，task 27b 完成 inventory + matrix 编译；
task 28 上 Orin 真机前还差三件套：(1) 性能基准，(2) 13-motor 规模端到端
集成 smoke，(3) 火焰图配套。本任务全部交付。

### 决定

1. **criterion bench 5 组**（`benches/feetech_bench.rs`）：
   - `encoding`：encode/decode_sign_magnitude + split/combine_bytes_le
   - `protocol_build`：build_ping/read/write/sync_read/sync_write
   - `parse`：parse_status + checksum
   - `bus`：单 motor read/write 端到端（mock transport）
   - `bus_sync`：13-motor sync_read / sync_write（XLeRobot 双臂 5+5 + 三轮 3 规模）
2. **集成 smoke 3 测试**（`tests/integration_smoke.rs`）：
   - 13-motor × 1000 循环 sync_read+sync_write（含 sign-magnitude 跨 0 边界）
   - 13-motor sync_read 解码负值（`-400` raw）
   - 13-motor handshake（每 motor PING + READ_DATA Model_Number）
3. **flamegraph 脚本**（`scripts/feetech_flamegraph.sh`）：
   - 封装 `cargo flamegraph -p robot-adapter-feetech --bench feetech_bench`
   - 默认 case = `sync_read_present_position_13motor`（task 28 真机最关心的 hot path）
   - 前置检查：`cargo-flamegraph` / `perf` / `perf_event_paranoid`
   - 输出 `target/criterion-flamegraph/<case>.svg`
4. **bench 数值入档** `docs/10_verification.md` 附录，作为协议层"无回归"的
   baseline。task 28 真机延迟瓶颈在 USB-RS485 wire RTT，**不**在 Rust 协议层。

### 实测 baseline（Linux x86_64 dev 机，release，criterion 0.5.1）

| 关键 case | 中位数 | 注 |
|---|---|---|
| `encode_sign_magnitude_pos` | 494 ps | 亚纳秒 |
| `decode_sign_magnitude` | 368 ps | |
| `build_sync_write_13motor_2byte` | 33.2 ns | 13 motor 包 build |
| `build_sync_read_13motor` | 14.5 ns | 13 motor 包 build |
| `parse_status_2byte` | 1.58 ns | |
| `bus.read_present_position_1motor` | 66.2 ns | 含 mock tx + ack rx + parse + drain |
| `bus.write_goal_position_1motor` | 73.7 ns | 同上 |
| **`bus.sync_read_present_position_13motor`** | **866 ns** | **task 28 真机最关心 hot path** |
| **`bus.sync_write_goal_position_13motor`** | **240 ns** | **同上**（不 ACK） |

**结论**：13-motor sync_read 端到端 866 ns ≈ 1 µs，远小于 50 Hz tick (20 ms)
预算。task 28 真机延迟全部在 USB-RS485 wire RTT（1 Mbps × packet ~50 B / motor →
几 ms 量级），不在协议层。

### 约束

1. **bench baseline 是相对值**：本机 dev 机数据仅作回归对照（CI 不卡阈值）。
   绝对延迟门槛以 task 28 Orin USB-RS485 实测为准。
2. **集成 smoke 的 1000 次循环非性能基准**：仅验"长循环不出错 + tx/rx buffer
   不泄漏"。性能数据走 criterion bench。
3. **flamegraph 脚本需要前置环境**：`cargo install flamegraph` + `perf` +
   `perf_event_paranoid <= 0`（`echo -1 | sudo tee ...`）。脚本内已检测 + 提示。
4. **CI 不自动跑 bench**：criterion 5+ 分钟一轮太重，本任务不挂 CI。task 28
   真机阶段或 v0.5 性能回归出现时手动跑。

### 影响面

| 文件 | 调整 |
|---|---|
| `Cargo.toml`（workspace） | `[workspace.dependencies]` 加 `criterion`（default-features=false + cargo_bench_support + html_reports） |
| `crates/robot-adapters/feetech/Cargo.toml` | dev-dependencies 加 `criterion`；`[[bench]] name="feetech_bench" harness=false` |
| `crates/robot-adapters/feetech/benches/feetech_bench.rs` | **新增**（5 组 criterion bench） |
| `crates/robot-adapters/feetech/tests/integration_smoke.rs` | **新增**（3 测试） |
| `scripts/feetech_flamegraph.sh` | **新增**（封装 cargo flamegraph） |
| `docs/10_verification.md` §10.2.1 | 加 3 行 task 27c harness |
| `docs/10_verification.md` 附录 | 加 feetech bench baseline 实测表 |
| `docs/10_verification.md` 变更记录 | 加 2026-04-24 条 |
| `docs/CHANGELOG.md` 本条 | 新增 |
| `docs/00_design.md` | **不动**（设计冻结） |

### 审阅

- 提议：Claude Code（2026-04-24 session 内 task 27c 入手时）
- 决定：用户 loosqk（2026-04-24 session 内"上真机前，冒烟测试，全量测试，
  性能测试和火焰图都要搞好"）
- 复审触发：
  - task 28 真机实测发现协议层 hot path 与本机 bench 偏差 > 10× → 重跑
    flamegraph 找瓶颈 + 更新 baseline 表
  - bench 数值在某 commit 突然退化 > 30% → 加 CI 夜间 bench compare job
  - 集成 smoke 1000 循环偶发失败（mock 行为变化 / inventory 链接问题）→ 加
    nightly schedule 跑 smoke 重复 100×

---

## 2026-04-24 · Step 7 task 28 feetech `real-hw` + Orin USB-RS485 单关节真机 bring-up（**状态：登记 / 实施 + 实测**）

### 涉及小节

- `docs/CHANGELOG.md` 2026-04-22 "workspace runtime 依赖白名单机制" → 新增
  `serialport` 进 `robot-adapter-feetech` 白名单（feature gated `real-hw`）
- `docs/CHANGELOG.md` 2026-04-24 task 27b → `AdapterBuildContext` 字段扩展
  规则首次行权（加 `address` + `motor_ids`）
- `docs/01_implementation_steps.md` Step 10 "Feetech 单关节" → bring-up 起步
  完成；剩 *真转电机* + *fail-safe 时延实测* 留下次 session
- `docs/handoffs/2026-04-24_6.md` §3 真机实测全文档
- 不动 `docs/00_design.md`

### Phase A 改动（commit `940c563`，本机准备）

1. **AdapterBuildContext 扩字段**（`robot-core::factory`）：
   - `address: Option<&'a str>` —— mock factory 忽略；feetech 必填
   - `motor_ids: &'a [u8]` —— 空 = fallback 1..=N；显式 = 按 joints 顺序对齐
   - 字段扩展严格遵守 task 27b 约定："旧 factory 忽略不读 / 新 factory 消费"
2. **`serialport = "4"` 加 workspace.dependencies**（仅 dev 机 + Linux 真机，
   依赖系统 `libudev-dev`；本条**进 robot-adapter-feetech 白名单**，仅
   `real-hw` feature 启用时 link，CI 默认 off 保持跨平台编译）
3. **`real-hw` feature**（`robot-adapter-feetech/Cargo.toml`）：拉
   `serialport` + 启用 `transport::real::RealSerialTransport` 模块 + 启用
   `factory::FeetechFactory` 模块（inventory submit）
4. **`xlerobot-real` combo feature**（`robot-s0/Cargo.toml`）：`xlerobot` +
   `robot-adapter-feetech/real-hw`；default 仍 `xlerobot`（mock-only），
   dev / CI 不强制 libudev-dev
5. **s0_daemon `build_adapter` 透传 address + motor_ids**：从
   `[[hardware]]` TOML extras 读 `motor_ids` 数组（`toml::Value::as_array`）
6. **`configs/xlerobot_task28_single_joint.toml`** 新增：`adapter =
   "feetech_sts3215"` + `address = "/dev/serial/by-id/usb-1a86_..."` +
   `joints = ["test_j1"]` + `motor_ids = [1]`

### Phase B 实测（本 session，详 `docs/handoffs/2026-04-24_6.md` §3）

- Orin (5.15.185-tegra aarch64 / JetPack 5) 上 `cargo build --release
  --no-default-features --features xlerobot-real -p robot-s0 --bin s0_daemon`
  通过（1m 36s clean rebuild）
- s0_daemon `--config configs/xlerobot_task28_single_joint.toml
  --smoke-duration 3s` 端到端通过：
  - `FeetechFactory::build` 正确实例化 + 打开 `/dev/serial/by-id/
    usb-1a86_USB_Single_Serial_5A7C121427-if00`
  - `FeetechBus::connect(true)` 内部 handshake：PING ID=1 + READ Model_Number
    → 收到 777 (STS3215)
  - 4 线程 spawn + state machine SAFE → READY（auto-Enable）
  - rt_thread 50 Hz tick 跑 `sync_read_by_id_into Present_Position/Velocity`
    在真 USB-RS485 wire 上 3s × 50Hz = 150 tick 全无错
  - clean shutdown + EStop latch 持久化 + exit 0
- 已确认：**5A7C121427 = bus1**（dimos `04_踩坑记录` 左臂总线，含 ID 1-8 +
  头部）

### 约束

1. **Orin build 两个坑**（已写 handoff §3.2）：
   - `.cargo/config.toml` 设 `rustc-wrapper = sccache`，但 Orin 没装 sccache
     → `CARGO_BUILD_RUSTC_WRAPPER=""` 绕开
   - cargo download 偶发 "transfer too slow"（默认 timeout 30s 不够）→
     `CARGO_HTTP_TIMEOUT=300 CARGO_NET_RETRY=10` + `cargo clean` 从干净状态
2. **address by-id 推荐 / ttyACM 不推荐**：udev 稳定路径 vs USB enumeration
   动态分配。configs 全部应用 by-id 路径。
3. **`motor_ids` 字段必须显式写**（除非 fallback 1..=N 适用）：dimos 多 bus
   场景（bus2 含底盘 ID 7-9）必须显式数组；factory 在 ID 长度不匹配时立刻
   `RobotError::InvalidParameter` 拒绝启动
4. **task 28 *未*覆盖**：真转电机 / kill -9 fail-safe 时延 / bus2 验证 /
   13-motor 同时跑 / Orin flamegraph / dimos 标定值接入。下次 session 待办
   完整列表见 `docs/handoffs/2026-04-24_6.md` §4（含用户提示的 dimos
   `~/repo/dimensional-applications/calibration/{left_arm,right_arm,head,
   base}.json` 4 份现成标定，格式与 `MotorCalibration` strut 字段对齐）

### `robot-adapter-feetech` 依赖白名单更新

| 依赖 | 范围 | feature gate | 用途 |
|---|---|---|---|
| `inventory = { workspace = true }` | dependencies | 默认 | factory 自注册（task 27b 已加，本次未变） |
| `serialport = { workspace = true, optional = true }` | dependencies | `real-hw` | RealSerialTransport USB-RS485 后端（**本次新增**） |

### 影响面

| 文件 | 调整 |
|---|---|
| `Cargo.toml`（workspace） | `[workspace.dependencies]` 加 `serialport = "4"` |
| `crates/robot-core/src/factory.rs` | `AdapterBuildContext` 加 `address` + `motor_ids` 字段 + 单测覆盖 |
| `crates/robot-adapters/feetech/Cargo.toml` | `inventory` deps 加；`serialport` optional + `[features] real-hw` |
| `crates/robot-adapters/feetech/src/transport.rs` | 加 `#[cfg(feature = "real-hw")] pub mod real { ... RealSerialTransport ... }` |
| `crates/robot-adapters/feetech/src/factory.rs` | **新增**（`FeetechFactory` + `inventory::submit!`，feature gated） |
| `crates/robot-adapters/feetech/src/lib.rs` | `pub mod factory` + `pub use FeetechFactory`（feature gated） |
| `crates/robot-s0/Cargo.toml` | `[features] xlerobot-real` 新增 |
| `crates/robot-s0/src/bin/s0_daemon.rs` | `build_adapter` 从 TOML 读 `address` + `extras["motor_ids"]` 数组，透传 ctx |
| `configs/xlerobot_task28_single_joint.toml` | **新增**（task 28 单关节测试 config） |
| `docs/handoffs/2026-04-24_6.md` | **新增**（Phase B 实测全过程 + 6 个待办） |
| `docs/handoffs/README.md` | 列表加 2026-04-24_6 行 |
| `docs/CHANGELOG.md` 本条 | 新增 |
| `docs/00_design.md` | **不动**（设计冻结） |

### 审阅

- 提议：Claude Code（2026-04-24 session 内 task 28 入手时）
- 决定：用户 loosqk（2026-04-24 session 内拍板继续 → "继续 27b/27c/28"
  → "刚才的优化点加上"（task 27c follow-up by-id）→ task 28 真机
  bring-up 跑通 → "orin 里的 dimos-deploy 有标定好的电机值" → handoff 加待办）
- 复审触发：
  - 真转电机 → 需要先 dump 当前 EEPROM Homing_Offset 备份再 write_calibration
    （防覆盖 dimos 已校）
  - bus2 (5A7C122340) 上线，加 second `[[hardware]]` 段 → 评估 s0_daemon
    多 bus 一进程拓扑 vs 多 instance（设计 §1.2 多 bus 方案 B 真落地）
  - dimos calibration JSON 接入 → robot-adapter-feetech 加 serde + serde_json
    依赖（白名单走本条 + CHANGELOG 后续条）

---

## 2026-04-25 · Step 7 task 29 feetech calibration JSON loader + silent clamp helper（**状态：完整通过（commit 1-5 + Orin 真机闭环 + 防跳 fix）**）

### 涉及小节

- `docs/CHANGELOG.md` 2026-04-24 task 28 §复审触发第 3 条 "dimos calibration JSON
  接入 → feetech 加 serde + serde_json 依赖" 行权 —— 本条是该行权的白名单登记 +
  实施首 commit
- `docs/CHANGELOG.md` 2026-04-22 "workspace runtime 依赖白名单机制" → 新增
  `serde` + `serde_json` 进 `robot-adapter-feetech` 依赖白名单
- `docs/01_implementation_steps.md` Step 10 "Feetech 单关节 bring-up" → task 28
  "真转电机" 待办的前置工作（commit 1/3：纯内存 loader；commit 2 给 adapter
  挂 follow-current + silent clamp；commit 3 Orin 三端闭环真转）
- 不动 `docs/00_design.md`

### 关键决定：**不写 EEPROM**

`MotorCalibration` JSON 已由 dimos 侧在 Orin EEPROM 上一次性写好（`Homing_Offset`
/ `Min_Position_Limit` / `Max_Position_Limit` 都是 EEPROM 区），本次 loader
**只读 + 内存缓存**，**不** 调 `crate::calibration::write_calibration` 覆盖 —— 覆盖 dimos
已校值的风险 > 每次冷启动写 EEPROM 的便利。真需要写 EEPROM 走单独的
`robotctl calibrate --apply` 手工命令（task 29 后续阶段再定义）。

用途：

1. adapter 启动时缓存 `range_min/max` 供热路径 silent clamp（1:1 对齐 dimos
   `_manipulator_base.py:422-426`）
2. 未来 raw ↔ rad 换算加 per-joint `direction` / `zero_offset_rad` 扩展（commit 1
   **不**实现，留给 commit 2 / 上层 adapter 配置）

### commit 1 改动范围（本 commit）

1. **`MotorCalibration` 加 serde derive**（`types.rs`）：
   - `#[derive(Serialize, Deserialize)]` + `#[serde(try_from = "MotorCalibrationRaw")]`
   - 内部 `MotorCalibrationRaw` private struct，`drive_mode` 可缺省默认 0（对齐
     dimos `entry.get("drive_mode", 0)`）
   - `TryFrom<MotorCalibrationRaw>` 调 `MotorCalibration::new` 走**同一条校验**
     （homing_offset ∈ [-2047, +2047]、range_min/max ∈ [0, 4095]）
2. **新增 `calibration_io` 模块**：
   - `load_file(path) -> HashMap<String, MotorCalibration>` —— 单文件
   - `load_and_merge(&[Path]) -> HashMap<String, MotorCalibration>` —— 多文件合并，
     motor name 重复 → 报 `CalibrationDuplicateMotor`（比 dimos "warning 后静默覆盖"
     **更严格**，避免配置错配难定位）
   - `clamp_raw(raw, lower, upper)` inline helper（1:1 移植 dimos
     `units.py::clamp_raw`）
   - 内部走 `serde_json::Value` 中转，per-entry 错误附带 motor name + file path
     （比 derive 默认的 "unknown field foo at line X" 更有定位价值）
3. **`FeetechError` 4 个新变体**：
   - `CalibrationIo { path, source: std::io::Error }`
   - `CalibrationParse { path, source: serde_json::Error }`
   - `CalibrationEntryInvalid { path, name, reason: String }`（serde 把 TryFrom 的
     `FeetechError::Display` 压成 String，无法拿回原结构化错误 —— 这里用 String 留
     serde 原文，保留 "range_max=5000 out of [0, 4095]" 这类可读信息）
   - `CalibrationDuplicateMotor { name, first: PathBuf, second: PathBuf }`
   - `From<FeetechError> for RobotError`：前两者归 `InvalidParameter("feetech
     calibration file")`；后两者归 `InvalidParameter("feetech calibration")`
4. **15 个单测**：load_file × 8（含 IO error / parse error / 非 object top-level /
   homing_offset 越界 / range 越界 / u16 溢出 / drive_mode 缺省）+ load_and_merge × 2
   （合并 / 重名）+ clamp_raw × 5（内 / 下越 / 上越 / 负数 / 等值退化）

### commit 2 改动范围（已落地）

1. **`FeetechAdapterConfig` 加 `calibrations: Vec<MotorCalibration>` 字段**（同序
   对齐 `joint_names`；空 Vec = 无标定）。`FeetechAdapter::new` 做长度校验（非空
   长度必须 == `joint_names.len()`），`limits` 依据 calibration 的 `range_min/max`
   raw → rad 换算；无 calibration 时保留原 ±π 默认
2. **`connect()` 加 `calibration.id == bus motor id` 校验**（防配错 `left_arm.json`
   被指给了 `right_arm` joints），不匹配立刻 `RobotError::InvalidParameter`
3. **`enable(true)` 1:1 移植 dimos `_manipulator_base.py:468-504` 防飞序列**：
   - sync_read Present_Position（每 motor 一 status 包）
   - sync_write Goal_Position ← Present（target 追平 current）
   - sleep 50 ms（常量 `ENABLE_SYNC_SETTLE`）消化总线
   - per-motor WRITE Torque_Enable=1 带 3× 重试 + 150 ms 间隔（常量
     `TORQUE_ENABLE_MAX_ATTEMPTS` / `TORQUE_ENABLE_RETRY_DELAY`）
   - 任一 motor 全部重试都失败 → 返回第一失败 motor 的错误
4. **`enable(false)` 也走 3× 重试**（对齐 enable(true) 一致性；无 sync_read 前置）
5. **`write_command` 热路径 silent clamp**：非空 calibration 时每 joint 走
   `clamp_raw(raw, range_min, range_max)`；空 calibration 时直写（dimos
   `_manipulator_base.py:422-426` 1:1）
6. **新增 public helper**：
   - `FeetechAdapter::sync_goal_to_present() -> Result<(), FeetechError>`（读当前
     Present，写回 Goal，不动 Torque）
   - `FeetechAdapter::hold_current_position() -> Result<(), RobotError>`（S0 在
     `CONTROL_LOST` 恢复时主动调，handoff §4.1 语义）
7. **私有 helper `write_torque_enable_with_retry(on: bool)`** 提取，避免 enable
   / disable 两条路径重复 3× retry loop
8. **Factory 侧传空 Vec**：`FeetechFactory::build` 暂时 `calibrations: Vec::new()`
   —— commit 3 扩 factory 从 config 读 `calibration_path` → 加载 → 传 adapter
9. **7 新单测**：enable(true) dimos 序列 / enable(false) 直写 / hold_current_position
   / write_command clamp 有 cal / write_command 无 cal 不 clamp / calibration-derived
   limits / 长度不匹配拒绝 / id 不匹配 connect 拒绝

### commit 3 改动范围（已落地 —— 代码 / config；Orin 真机闭环待下一步）

1. **`AdapterBuildContext` 加 `calibration_paths: &'a [String]` 字段**：
   - 字段语义：一条总线的标定 JSON 路径数组，dimos bus1 = left_arm.json +
     head.json（双文件合并），bus2 = right_arm.json + base.json
   - 空数组 = "无标定"；非空 → factory 负责 `load_and_merge` + 按 `joint_names`
     顺序 lookup
   - 遵循 task 27b 字段扩展规则：旧 factory 忽略不读（mock / 其他 non-feetech
     adapter 不受影响）；新 factory 按需消费
   - `factory.rs` 内 `build_context_passes_through_fields` 测试同步扩
2. **`FeetechFactory::build` 消费 calibration_paths**：
   - `ctx.calibration_paths.is_empty()` → `calibrations: Vec::new()` 传给 adapter
   - 非空 → `calibration_io::load_and_merge(ctx.calibration_paths)` 合并所有文件
     → `motor_name → MotorCalibration` map
   - 按 `joint_names` 顺序 lookup；任一 joint 查不到 → `RobotError::
     InvalidParameter("feetech: calibration missing motor")` + stderr 打印
     available keys 辅助定位
   - build 完成日志附 `calibration=loaded|none` 字段
3. **`s0_daemon::build_adapter` 读 TOML extras**：
   - `[[hardware]] calibration_paths = ["..."]` 字段通过 `hw.extras` 读取
     （`as_array()` + `as_str()` filter_map）
   - 未设置 / 空数组 → 透传空 Vec，factory 不加载
   - fallback 路径（CLI-only mock）保持空 calibration_paths
4. **新增 config 文件**：
   - `configs/xlerobot_task29_single_joint.toml`：基于 task28 config，joint 名
     改为 dimos 标准 `left_arm_shoulder_pan`（对齐 `left_arm.json` 第一条），
     加 `calibration_paths` 指 Orin 上 `/home/loosqk/repo/dimensional-applications
     /calibration/{left_arm,head}.json`（bus1 完整合并）
   - 文件头长注释说明：calibration 只读 / 不写 EEPROM / follow-current 防飞 /
     bus1 拓扑

### commit 3 Orin 端到端闭环实测（**已完整通过**，详见 `docs/handoffs/2026-04-25_1.md` §3）

**bring-up 策略调整**（用户 2026-04-25 指令"只动末端夹爪"）：
原 config 选 `left_arm_shoulder_pan` (ID=1) —— 机械臂未装到小车时旋转 shoulder
风险大；切换到 `left_arm_gripper` (ID=6)，动的是末端夹爪开合，无机械风险。
config 改动在 commit `82e7a13`（task29 config 切换）。

**三端闭环实测结果**：

| 验证项 | 结果 |
|---|---|
| `calibration=loaded`（factory 日志） | ✅ |
| auto-enable 序列不飞（smoke 5s 无 S1） | ✅ state=Ready，夹爪不动 |
| state 升 Enabled（control_link Ok） | ✅ S1 心跳 + command 正常接收 |
| Goal_Position 真物理运动 ±5° | ✅ 用户肉眼物理确认夹爪真的在开合 |
| 正弦振幅跟踪精度 | ✅ 实测 0.083 vs 目标 0.087 rad，误差 4.6% |
| 正弦周期精度 | ✅ 4 s / cycle = 0.25 Hz，3 个完整周期 |
| `CtrlLostHold` 子态触发（S1 duration 到） | ✅ output_mode: Hold |
| `hold_current_position` helper 工作 | ✅ 12 个采样锁死 `-0.008` rad，不 drift / 不归零 |
| s0 / s1 / robotctl clean shutdown | ✅ |

实测命令（供复现 / 后续验证参考）：

```bash
./target/release/s0_daemon \
  --config configs/xlerobot_task29_single_joint.toml \
  --latch-path /tmp/robot-task29/estop_latch --smoke-duration 18s &
sleep 2
./target/release/mock_s1 --robot-id xlerobot_task29 \
  --hardware-id left_arm_single --joints 1 \
  --amplitude 0.087 --frequency 0.25 --duration 12s &
sleep 1
timeout 14 ./target/release/robotctl watch --robot-id xlerobot_task29 \
  --hardware-id left_arm_single --interval-ms 250
```

**未做**（留更后续 session）：
- bus2（5A7C122340 右臂 + 底盘）多 motor 同时闭环
- 13-motor 全栈 sync_read（双 bus 同时）
- fail-safe 时延实测（kill -9 s0_daemon → motor torque disable 时延，设计
  §6.7 + §10.1 要求 < 500 ms）
- `robotctl calibrate --apply` 工具（如果未来需要 Rust 侧主动写 EEPROM）
- Orin flamegraph aarch64 hot path（任务 27c 火焰图是本机 mock，真机分布可能不同）

### commit 4/5 防跳 follow-up（本 session 后续发现 + 修复）

三端闭环真机看到的瑕疵：**从闭合位启动后，mock_s1 接管瞬间电机从 raw ~1400
（闭合）跳到 raw 2047（center）≈ 36° "张开到一半"**。先做了两条独立修复：

- **commit 4**（`61d2b76` · adapter defensive）：`FeetechAdapter::sync_goal_to_
  present` 加 readback + retry：写完 Goal 后 `sleep 20 ms → sync_read Goal_Position`
  验证，若差 > `SYNC_GOAL_TOLERANCE (20 tick)` → 重写最多 3 次；失败返回
  `FeetechError::SyncGoalReadbackMismatch` + 每次 attempt eprintln 对比日志。
  最初假设 "Torque=0 时 sync_write 被忽略"，但用户澄清**从没断电** → adapter
  层不是 root cause。**保留作 defensive 兜底**（真冷启动 + 固件异常时仍有用），
  3 新单测覆盖 retry / tolerance / max-attempts 边界。
- **commit 5**（`25658f0` · **真 root cause fix**）：mock_s1 加 `--anchor-timeout-ms`
  参数（默认 1000）。启动时订阅 S0 state topic 读一帧 `positions[0..joints]`
  做正弦 bias，`target[i] = bias[i] + amp*sin(phase + i*π/4)`。原公式 `amp*sin(phase)`
  第一帧 sin(0)=0 rad = raw 2047 (center) → motor 被硬拉到中点；锚定后第一
  帧 Goal=bias=当前位置 → **motor 不动**。超时 / 订阅失败 fallback 旧行为 +
  warning。

**commit 5 真机验证**（bus1 left_arm_gripper，夹爪已在 raw 1418 闭合位）：

| 验证项 | 结果 |
|---|---|
| mock_s1 anchor log: `bias=[-0.962]` | ✅ 对应 raw ≈ 1420 |
| 第一帧 pos = -0.882 rad | ✅ 不是 0.080 (center)，完全无 36° 跳变 |
| 正弦围绕 bias 摆动 `[-0.969, -0.882]` | ✅ 幅值 ±0.044（限位前有效开合） |
| 负峰被 silent clamp 卡在 range_min (-0.98) | ✅ clamp 同时生效，验证 commit 2 |
| S1 duration 结束 → `hold_current` 锁死 -0.969 × 14 次 | ✅ hold_current 不 drift |
| 用户肉眼："没跳，紧紧闭合没动" | ✅ bias 太接近 range_min → 负半周期物理被 clamp 限位，视觉表现为"挤压闭合" |

**commit 5 改动范围**：
- `crates/robot-s1/src/bin/mock_s1.rs`：加 `--anchor-timeout-ms <MS>` CLI（默认
  1000，0=禁用）；启动时开 state subscriber + 10 ms 间隔 poll；主循环正弦公
  式加 bias；三条 fallback 路径（subscriber open 失败 / view decode 失败 /
  轮询 timeout）全部 `warning + bias=0` 保留旧行为

### commit 5 已知小瑕疵（非 regression）

- `robotctl watch` 用 `timeout 14` 被 SIGTERM kill 时 segfault。全程 13s 数据
  正常，是 iceoryx2 subscriber drop 顺序问题，**不影响功能**。记 task-30
  候选：robotctl signal handler 提早 drop subscriber 或改 `ctrlc` crate 优雅
  退出。

### 依赖白名单更新（robot-adapter-feetech）

| 依赖 | 范围 | feature gate | 用途 |
|---|---|---|---|
| `serde = { workspace = true }` | dependencies | 默认 | `MotorCalibration` try_from + derive（**本次新增**） |
| `serde_json = { workspace = true }` | dependencies | 默认 | JSON 读文件 + per-entry 错误定位（**本次新增**） |
| `tempfile = { workspace = true }` | dev-dependencies | 默认 | 单测 NamedTempFile 写 JSON fixture（**本次新增**） |

### 约束

1. **JSON 格式与 dimos 1:1 兼容**（`scripts/calibrate.py` / `common/calibration.py`
   输出）：顶层 `{motor_name: {id, drive_mode?, homing_offset, range_min, range_max}}`；
   未来格式变更走 CHANGELOG + 对应 dimos 同步
2. **loader 纯只读 / 纯内存**，**不**触 motor EEPROM；`write_calibration` 路径仍在
   `crate::calibration`，调用方自己控制（未来 `robotctl calibrate --apply` 入口）
3. **重名 = 错误**（与 dimos warning 行为有意不同）；静默覆盖的调试成本 > 明确
   拒绝的 config 修正成本
4. **`MotorCalibration::new` 不校验 `id`**（id ∈ [1, 253] 校验在 `Motor::new`，
   calibration 不直接构造 Motor）—— loader 不重复这层，保留 dimos 的松耦合
5. **raw ↔ rad 换算本 commit 不改**：`adapter.rs` 的 `raw_to_rad` / `rad_to_raw`
   保留纯线性（无 direction / zero_offset_rad）；per-joint 偏移扩展留 commit 2
   实际需要时再加，避免过度设计

### 影响面

| 文件 | 调整 | commit |
|---|---|---|
| `Cargo.toml`（workspace） | `[workspace.dependencies]` 加 `serde_json = "1"` + 白名单注释 | 1 |
| `crates/robot-adapters/feetech/Cargo.toml` | `[dependencies]` 加 `serde` + `serde_json`；`[dev-dependencies]` 加 `tempfile` | 1 |
| `crates/robot-adapters/feetech/src/types.rs` | `MotorCalibration` 加 `#[derive(Serialize, Deserialize)]` + `#[serde(try_from)]` + 私有 `MotorCalibrationRaw` struct + `TryFrom` impl | 1 |
| `crates/robot-adapters/feetech/src/calibration_io.rs` | **新增**（`load_file` + `load_and_merge` + `clamp_raw` + 15 单测） | 1 |
| `crates/robot-adapters/feetech/src/error.rs` | `FeetechError` 加 4 变体 + `From` impl 对应更新 | 1 |
| `crates/robot-adapters/feetech/src/lib.rs` | `pub mod calibration_io` + `pub use calibration_io::{clamp_raw, load_and_merge, load_file}` | 1 |
| `crates/robot-s0/tests/rt_guard_soak.rs` | pre-existing 2 处 `clippy::doc_markdown` 错误（`tick_count` / `SCHED_FIFO`）顺手补 backtick | 1 |
| `crates/robot-adapters/feetech/src/adapter.rs` | `FeetechAdapterConfig` 加 `calibrations`；adapter 存字段 + calibration-derived limits；`connect()` 加 id 校验；`enable(true)` 按 dimos 序列 + 50 ms 消化 + 3× 重试；`enable(false)` 同 3× 重试；silent clamp in `write_command`；新加 `sync_goal_to_present` + `hold_current_position` public helper + `write_torque_enable_with_retry` 私有 helper；新增 7 单测 | 2 |
| `crates/robot-adapters/feetech/src/factory.rs` | `FeetechFactory::build` 构造 `FeetechAdapterConfig` 时传 `calibrations: Vec::new()`（commit 3 再扩读 calibration_path） | 2 |
| `crates/robot-core/src/factory.rs` | `AdapterBuildContext` 加 `calibration_paths: &'a [String]`；测试同步扩 | 3 |
| `crates/robot-adapters/feetech/src/factory.rs` | 扩读 `ctx.calibration_paths` → `load_and_merge` → 按 `joint_names` 顺序 lookup → 传 adapter；build 日志附 `calibration=loaded|none` | 3 |
| `crates/robot-s0/src/bin/s0_daemon.rs` | `build_adapter` 从 TOML `hw.extras["calibration_paths"]` 读数组 → 透传 `AdapterBuildContext.calibration_paths` | 3 |
| `configs/xlerobot_task29_single_joint.toml` | **新增**（task 29 单关节 + 完整 bus1 calibration 合并 config） | 3 |
| `docs/CHANGELOG.md` 本条 | 新增 / 更新 | 1 + 2 + 3 |
| `docs/00_design.md` | **不动**（设计冻结） | — |

### 验证（task 29 全部完成后）

- `cargo fmt --all --check` ✅
- `cargo clippy --all --all-targets -- -D warnings` ✅（零警告）
- `cargo test --all` ✅ 全部绿；feetech 单测 **105 pass**（commit 1 的 98 + commit 2 的 7）；
  workspace 总 **336 pass** / 0 fail / 7 ignored
- matrix features 本机 build：`mock-only` / `xlerobot` / `atom01` 三个组合 ✅
- **`xlerobot-real` + Orin 真机端到端闭环**：✅ bus1 `left_arm_gripper` (ID=6)
  真转电机通过（±5° 正弦 × 3 周期 + `CtrlLostHold` 子态 hold_current_position
  语义确认）。详 `docs/handoffs/2026-04-25_1.md` §3

### 审阅

- 提议：Claude Code（2026-04-25 session 开场；用户 2026-04-24 handoff §4.6
  提示 "接入 dimos 现成标定值"）
- 决定：用户 loosqk（2026-04-25 本 session 拍板 "不覆盖 EEPROM，仅内存层
  loader"，"启动时 follow-current 防飞"，"range 超限走 silent clamp 对齐 dimos"）
- 复审触发：
  - adapter 真集成 calibration（commit 2）→ 需定义 `FeetechAdapter::connect` 时
    `MotorCalibration[]` 注入路径 + per-joint direction / zero_offset_rad 是否
    进 crate
  - `robotctl calibrate --apply` 工具 → 如果未来需要一键写 EEPROM，重新激活
    `crate::calibration::write_calibration` 这条路
  - dimos JSON 格式变更 → 后续 CHANGELOG 登记 + 同步 parser

---

## 2026-04-25 · task-32 日志骨架（**状态：登记 / 骨架落地**）

### 涉及小节

- `docs/CHANGELOG.md` 2026-04-22 "workspace runtime 依赖白名单机制" → 新增
  `tracing` / `tracing-subscriber` / `ctor` 进 workspace 白名单
- `docs/00_design.md` 不动（本 task 只增基础设施，不改设计语义）
- 新增规范文档 `docs/logging.md`（字段词汇表 + 级别约定 + RT 铁律）

### 决定

- **事实标准库**：`tracing` + `tracing-subscriber`（Rust 版 spdlog），
  `thiserror` 继续做 error derive 标准（已在用）。
- **时间戳**：wall clock RFC3339 UTC + monotonic `[+secs.ns s]`（`Instant::
  now().duration_since(start)`，`CLOCK_MONOTONIC_RAW` 原理一致，NTP / 闰秒
  无感）。手写 Howard Hinnant 日期算法，零外部 chrono/time 依赖。
- **字段命名统一词汇表**（`docs/logging.md` 权威）：`robot_id` / `hardware_id`
  / `motor_id` / `joint` / `adapter` / `err` (Debug) / `attempt` /
  `duration_ms` 等，新增字段必须走 CHANGELOG + 更新 logging.md。
- **自动初始化**：`ctor` crate `#[ctor::ctor]` 在 `main` 运行前自动装 subscriber
  —— **bin 的 main 里不用写 init 代码**，只要 `Cargo.toml` 依赖 `robot-logging`。
  可选 `let _ = robot_logging::init()` 作 self-documenting anchor（幂等）。
- **RT 铁律保留**：RT thread 内**禁用**任何 `tracing::*!` 调用（含 literal
  message）—— tracing Event 构造可能 alloc，违反 §4.9。RT 路径走 metrics
  atomic counter + 非 RT 线程定期采样。

### 影响面

| 文件 | 调整 |
|---|---|
| `Cargo.toml`（workspace） | `members` 加 `crates/robot-logging`；deps 加 `tracing` / `tracing-subscriber` / `ctor`；`robot-logging` path dep |
| `crates/robot-logging/` | **新增 crate**（135 行 lib.rs + Cargo.toml）：`init()` 公共入口 + `WallAndMono` FormatTime + `#[ctor]` 自动初始化 + 3 单测（civil_from_days / RFC3339 / init 幂等） |
| `crates/robot-s0/Cargo.toml` | 加 `robot-logging` + `tracing` deps |
| `crates/robot-s0/src/bin/s0_daemon.rs` | `main` 顶部加 `let _ = robot_logging::init();`（self-documenting anchor，ctor 已自动跑过） |
| `crates/robot-s1/Cargo.toml` | 同上 |
| `crates/robot-s1/src/bin/mock_s1.rs` | 同上 |
| `crates/robot-cli/Cargo.toml` | 同上 |
| `crates/robot-cli/src/bin/robotctl.rs` | 同上 |
| `crates/robot-adapters/feetech/Cargo.toml` | 加 `tracing` dep（lib 用 macros）；`robot-logging` optional + 进 `real-hw` feature（feetech_poke example 需要） |
| `crates/robot-adapters/feetech/examples/feetech_poke.rs` | 同上（anchor init） |
| `docs/logging.md` | **新增** 规范文档 |
| `docs/CHANGELOG.md` 本条 | 新增 |

### 依赖白名单更新

| 依赖 | 范围 | 用途 |
|---|---|---|
| `tracing = "0.1"` | workspace.dependencies | 结构化日志 macros；**所有 lib / bin 允许用**（RT thread 除外） |
| `tracing-subscriber = { version = "0.3", default-features = false, features = [...] }` | workspace.dependencies | subscriber runtime；**只允许 `robot-logging` crate 用** |
| `ctor = "0.3"` | workspace.dependencies | 自动初始化 hook；**只允许 `robot-logging` crate 用** |

### 约束

1. **本 commit 是骨架 + 规范**，**现有 `eprintln!` / `println!` 调用不动** ——
   全量替换分后续多个小 commit 做（按 crate 切分，每次 30-60 min），避免一次
   改动过大。大致有 100+ callsite 需要迁移。
2. **新代码默认用 tracing**：task-32 之后所有新加的 log 必须用 `tracing::*!`
   + 统一词汇表。reviewer 看到 `eprintln!` 打非 debug 信息（非 fatal error
   stderr / 非 --help 输出）直接 block。
3. **`tracing-journald` 暂不接**：systemd 下 JSON 模式走 stdout → journald 能
   抓结构化字段（`PRIORITY=` 等）。接 native journald 留后续评估（需要比较
   JSON 抓取 vs native binary protocol 的运维友好度）。
4. **Error 结构保留**：打错误一律 `error!(?err, "...")`（`?` 走 Debug），不用
   `error!(err = %err, ...)` 或 `format!("{err}")` 丢结构。

### 验证

- `cargo fmt --all --check` ✅
- `cargo clippy --all --all-targets -- -D warnings` ✅（含 `--features real-hw`）
- `cargo test --all` ✅（robot-logging 新增 3 单测全绿；workspace 共 339 pass）
- `cargo build --all` ✅（matrix 4 组合无回归）

### 审阅

- 提议 → 决定：用户 loosqk（2026-04-25 本 session：`整个项目的日志也要用成熟
  的高性能日志库，就像 spdlog 之于 c++` → `日志格式也要统一方便查看` → `日志
  还要加上 monotonic 时间` → `用事实性的标准 error，例如 thiserror？都用最新
  版本` → `有统一 init 的吗，这要一个一个写`）
- 复审触发：
  - 全量替换 `eprintln!` 时发现词汇表缺字段 → 更新 `docs/logging.md` + CHANGELOG
  - RT 路径日志需求（v0.3+）→ 专门 task 设计 lock-free ring buffer + 异步 flush
  - `tracing-journald` 真切到 native binary protocol → 新 CHANGELOG 条目

---

## 2026-04-25 · task-31 B'a 看门狗扩展骨架：`robot-health` crate（**状态：登记 / 骨架落地**）

### 涉及小节

- session 路线图步骤 4（handoff 2026-04-25_1 §8 / §7.6.3 待办）
- `docs/logging.md` 字段词汇表 → 新增 `cpu_usage_pct` / `load_1m` / `load_5m` /
  `load_15m` / `mem_total_kib` / `mem_available_kib` / `mem_used_pct`
- 依赖 task-32（同日 commit `9cdeb1b`）的日志骨架 —— 本 task 产物直接用
  `tracing::info!` 输出

### 决定

- **独立 crate `robot-health`**：不进 `robot-core`（§4.2 零 runtime 依赖），
  也不强绑 `robot-s0`（其他 daemon 也能用）。
- **只做系统级 + 骨架**：CPU / memory / load 三项，5 秒周期；**不做** per-
  process / top5 / 配置化 —— 留 B'b。
- **手写 `/proc` parser**：不依赖 `sysinfo` / `procfs` crate，纯整数 / 字符串
  解析 + 22 单测（fixture + Linux live read）。macOS stub `Err` → 上层 fallback
  0 让 dev 机编译。
- **非 RT**：`SCHED_OTHER` 默认优先级，秒级周期，**不**挤占 `s0_rt_thread`。
- **`s0_daemon` 默认集成**：spawn 一个 `_health_handle`（Drop 自动 join），
  早失败分支也会触发（main 作用域内的 local），**不需要**改 `graceful_stop`。

### CPU 使用率差分算法

`/proc/stat` 第一行 `cpu user nice system idle iowait irq softirq steal ...`
（jiffies）。两帧差分：`busy_delta / total_delta * 100`。单帧无意义（返回
`0.0` 占位）。本机 12s smoke 实测：帧 1 = 0.0%、帧 2 = 8.91%、帧 3 = 8.98%。

### 影响面

| 文件 | 调整 |
|---|---|
| `Cargo.toml`（workspace） | members 加 `crates/robot-health`；path dep 加 |
| `crates/robot-health/` | **新增 crate**（2 文件 + 22 单测 + 1 doctest）：`lib.rs`（SystemSnapshot / MonitorConfig / spawn_monitor / MonitorHandle）；`proc.rs`（CpuTimes / MemInfo / LoadAvg + 纯整数 parser）|
| `crates/robot-s0/Cargo.toml` | 加 `robot-health` dep |
| `crates/robot-s0/src/bin/s0_daemon.rs` | 第 4.5 阶段（watchdog 之后）加 3 行 spawn_monitor + 5 行注释 |
| `docs/CHANGELOG.md` 本条 | 新增 |

### 验证

- `cargo fmt --all --check` ✅
- `cargo clippy --all --all-targets -- -D warnings` ✅（含 `--features real-hw`）
- `cargo test --all` ✅（robot-health 22 单测 + 1 doctest；workspace 共 361 pass）
- matrix features 本机 build ✅（4 组合）
- 本机 12s smoke：health 3 帧 tracing 输出，CPU 差分稳态 ~9%

### B'b 待做

- per-process 采集（`/proc/<pid>/stat` + `status`）：interested processes RSS / CPU%
- 全系统 top 5 by CPU
- TOML `[monitor]` 段：`interested_processes` 列表 + `interval_secs`
- 可选：`/<robot_id>/s0/system_health` iceoryx2 topic（定义 FlatBuffers schema，
  **先不做**，等真有跨进程订阅需求再加）
- B'c 候选：CPU > 阈值 → `Event::RtOverrun` → state_machine 进 `Fault`；依赖
  task-30 state_machine ↔ adapter 执行链路先接通

### 审阅

- 提议 → 决定：用户 loosqk（2026-04-25 `看门狗要盯着系统总体资源占用以及使
  用率，top5 进程，以及我的每个服务进程资源占用`）

---

## 2026-04-25 · task-31 B'b 看门狗扩展 per-process + top5 + TOML（**状态：登记 / B'b 完成**）

### 涉及小节

- 本条延伸 B'a（同日 commit `b59e286`）；两条合在一起 = task-31 B' 完整骨架
- `docs/logging.md` 词汇表 → 新增 `cpu_usage_pct` / `load_*` / `mem_*` /
  `interested` / `top_by_cpu`
- `robot-s0` TOML schema 加 `[monitor]` 段

### 决定

- **per-process CPU 语义与 `top` 一致**：`delta / total_cpu_delta * num_cpus *
  100`，单核 100% 满载 → 100.0；多核同时 → 超 100。不归一化到 (0, 100]，保留
  直觉语义。
- **`/proc/<pid>/stat` `comm` 字段解析坑**：括号可含空格 + 嵌入括号（`(kworker
  u:1-events)` / `(python 3)`）。解析走 `rfind(')')` 找最后一个右括号后再切字段。
  手写 parser + 4 个专项单测（simple / tricky parens / no parens / short）。
- **HashMap 生命周期管理**：`collect_process_snapshots` 每帧替换 `last_pid_cpu`
  为当前帧存活 pid，**短命进程 pid GC**，长期运行 HashMap 不增长。
- **TOML `[monitor]` 段**：三字段 `interval_secs` / `top_n` / `interested_
  processes`，全部可缺省（走 `Default`，兼容 v0.1 无 `[monitor]` 段 config）。
- **tracing 输出格式**：`interested` / `top_by_cpu` 做 `Display` 字段（一行展示：
  `pid:comm@cpu%[rssM],...`），不打散为嵌套字段 —— 保证"一行看全貌"，JSON 模式
  下为 embedded string（不嵌套 array）。

### 实测（本机 smoke 8s）

```
system health cpu_usage_pct=0.0  ... mem_used_pct=84.55
  interested=825550:s0_daemon@0.0%[20M]
  top_by_cpu=1:systemd@0.0%[10M],2:kthreadd@0.0%[0M],3:pool_workqueue_release@0.0%[0M]

system health cpu_usage_pct=8.83  ... mem_used_pct=84.95
  interested=825550:s0_daemon@1.5%[20M]
  top_by_cpu=3297879:python3@111.7%[38131M],2345972:python3@106.7%[32940M],3131702:rustdesk@20.4%[400M]
```

系统 CPU 差分 8.8%，`s0_daemon` 自身 1.5%（cargo build 负载已散去），`top_by_cpu`
捕捉到本机两个 python3（训练跑满多核 ≈ 110%）+ rustdesk (20%)。完全符合 `top`
视角。

### 影响面

| 文件 | 调整 |
|---|---|
| `crates/robot-health/src/proc.rs` | 加 `PidStatRaw` / `parse_pid_stat` / `read_pid_stat` / `parse_vm_rss_kib` / `read_pid_rss_kib` / `list_pids`；7 新单测（含 tricky comm parens / live self-pid read） |
| `crates/robot-health/src/lib.rs` | `ProcessSnapshot` 新增；`SystemSnapshot` 扩 `interested` + `top_by_cpu`；`MonitorConfig` 扩 `interested_processes` + `top_n`；`collect_process_snapshots` / `per_process_cpu_pct` / `num_cpus_online` / `parse_cpu_online` / `summarize` + 10 新单测 |
| `crates/robot-s0/src/config_loader.rs` | `RobotConfigToml` 加 `monitor: MonitorSection`（`interval_secs` / `top_n` / `interested_processes`，全部 `#[serde(default)]`） |
| `crates/robot-s0/src/bin/s0_daemon.rs` | spawn_monitor 从 `ConfigLoader.parsed.monitor` 构建 `MonitorConfig`，无 `[monitor]` 段 fallback `Default::default()` |
| `docs/logging.md` | 词汇表补 7 个字段（cpu / load / mem / interested / top_by_cpu）|
| `docs/CHANGELOG.md` 本条 | 新增 |

### 验证

- `cargo fmt --all --check` ✅
- `cargo clippy --all --all-targets -- -D warnings` ✅（含 `--features real-hw`）
- `cargo test --all` ✅（robot-health **42 单测 + 1 doctest 全绿**；workspace 共 381 pass）
- matrix features 本机 build ✅（4 组合）
- 本机 8s smoke：`[monitor]` 段 TOML 正确传到 spawn_monitor，interested / top5
  输出正确

### B'c 待做（supervisor + 重启策略）

用户 2026-04-25 明确 **走方案 C**（`B'c 同意c方案`）：

**方案 C：监控 + 决策上报，systemd 执行重启**

- `robot-health` 层**监控** CPU / RSS / 状态（本 B'a + B'b 已完成）
- **新增**：TOML `[monitor.rules]` 段声明阈值（`max_cpu_pct_sustained` / `max_rss_kib`
  / `restart_on_zombie` / `cooldown_secs`），sampler 发现违规 → `tracing::warn!`
  + 发 `Event::ServiceUnhealthy` 到 state_machine（待 task-30 执行链路接通后可用）
- **真动作** 仍由 systemd 执行：可选调 `systemctl restart <service>`（需要
  `robot-health` / `s0_daemon` 有 PolicyKit 权限）
- **不做**：自己 fork/exec 启动服务、自己 reap zombie、自己管 PID 1 语义（与
  §6.7 "systemd 是最外层看门狗" 冲突 → 方案 A 已被否）

工作量估计：~2 小时（rules + warn 级别联动）。留独立 task-31 B'c commit。

### 审阅

- 提议 → 决定：用户 loosqk（2026-04-25 `还要考虑稳定性，以及方案[可能所有
  服务从看门狗启动，如果是非正常退出，重启策略]` → 对齐三方案 → `B'c 同意c方案`）
- 复审触发：
  - B'c 实施时 supervisor ↔ state_machine 交互 → 依赖 task-30 先落地
  - PolicyKit / systemd D-Bus 权限粒度评估 → 可能改 systemd unit 配置

---

## 2026-04-25 · task-30 设计方案：state_machine ↔ adapter 执行链路（**状态：草案 / 等 review**）

### 涉及小节

- 路线图步骤 5（handoff 2026-04-25_1 §8 "task-30 高优先候选"）
- `docs/handoffs/2026-04-25_1.md` §7.5.2 架构缺口记录的延续
- **不动 `docs/00_design.md`**（§4.9 / §6.7 / §1.4 全保）

### 产出

- 新增 `docs/designs/task-30_state_machine_adapter_link.md`（523 行设计文档）
  - §1 问题陈述：当前 state_machine 只反映状态不驱动 adapter；EStop 不断扭是
    安全级 bug；ControlMode::Stop 被 adapter 忽略
  - §2 设计原则（硬约束）：RT 铁律 / systemd supervisor / trait 外部签名 / panic
    abort / 单测 100% 向后兼容 / 允许增量
  - §3 架构图：当前 vs task-30 后（io_helper thread 接管 adapter 所有权）
  - §4 候选方案 A-F 对比，**推荐方案 F**（adapter 在 io_helper 独占 + RT 读写
    lock-free POD）
  - §5 详细设计：AdapterReq/Resp + AdapterCtrlClient + AdapterCtrlServer
    + HotShared；`ManipulatorAdapter::hold_current_position` 加默认 no-op
  - §6 迁移计划 M1-M4 四个独立 commit（1 + 0.5 + 2 + 1 ≈ 4.5 h）
  - §7 测试覆盖：单测 / integration / soak / Orin 真机
  - §8 风险 R1-R4 + 回滚策略（每 commit 独立 revert）
  - §9 不做的事
  - §10 决议请求（5 个问题请 user 回复）

### 5 个决议请求（等 user review）

1. 整体方向方案 F 是否 approve？
2. 迁移计划 M1-M4 分 commit 是否合理？
3. `hold_current_position` trait 默认 no-op 是否接受？
4. rt_thread 降级为纯数据通路（5.5.b，一帧 ≤20 ms 延迟）是否接受？
5. feature flag `state_machine_adapter_link` 是否需要？默认 on / off？

### 审阅

- 提议：Claude Code（2026-04-25 路线图步骤 5 "A 设计方案"）
- 等决：用户 loosqk

### v2 修订（2026-04-25 user review 后）

User 给出 12 条 critical 反馈，全部接受。文档从 523 行 → ~860 行重写覆盖：

**核心 framing 变化**：v1 解决"谁拥有 adapter"；v2 必须同时解决"什么时候允
许 `latest_cmd` 写硬件" —— 后者才是安全闭环。原 v1 的 `hold_current_position`
设计**有破洞**：io_helper 仍然每 tick 转发 stale `latest_cmd`，会冲掉 hold
语义。

**主要 v2 加 / 改 / 删**：
- ➕ `§4 OutputPolicy` 新章节：7 variant（Disabled / ReadyHold / ActiveForward
  / HoldCurrent / ZeroCommand / StopTorque）+ `S0State → OutputPolicy` 映射表 +
  arc-swap 实现；adapter_io_thread 严格按 policy 决定是否转发 `latest_cmd`
- ➕ `§5.1` AdapterReq 拆 `Emergency` / `Normal` 双 channel + `estop_requested:
  AtomicBool` 双兜底；emergency 不被 normal 阻塞
- ➕ `§5.4` server loop 改 `crossbeam_channel::select!` + deadline，**防 busy
  spin**（断连不 CPU 100%）；明确不靠串口 RTT 当节拍器
- ➕ `§5.5` `tick_hardware` 按 OutputPolicy 决定输出；`cmd.mode = Stop` 真触发
  emergency safety_stop；mode 变化时调 set_mode 一次（不每 tick 调）
- ➕ `§5.7` `hold_current_position` trait 默认改 `Err(RobotError::Unsupported)`
  （**不**默认 no-op）；mock 显式 no-op；fallback：hold 失败 → safety_stop
- ➕ `§6 OutputSafetyState` 新章节：`TorqueOffConfirmed` / `TorqueOn` /
  `TorqueUnknown` / `StopFailed`；新 `FaultCode::SafetyStopFailed` /
  `AdapterUncertain`；对外 state topic 加 `safety_state` 字段（与现有
  `OutputMode` 正交：意图 vs 真实硬件状态）
- ➕ `§7 SIGTERM graceful` 新章节：明确不依赖 Drop / panic = abort 不跑 Drop；
  shutdown 走 emergency channel + bounded timeout；SIGKILL / panic 必须靠硬件层
- ➕ `§8 测试覆盖` 加 9 条用例：stale gate / Stop trigger safety / mode change
  once / emergency preempt / no busy spin / shutdown / adapter timeout /
  TorqueOffConfirmed E2E / OutputSafetyState 上报
- ➖ 删 v1 `§5.5.a` "RT 直调 adapter" 退路（user review §7：是方案 F 否定式）
- ➖ 改名 `io_helper` → `adapter_io_thread`（user review §7：原名太轻）
- 🔄 迁移计划 M1-M4 拆为 **M1-M5**（user review §6：M3 单独合并行为退化），
  feature flag 时间线明确：M1-M3 不存在 / M4 默认 off / M5 真机 smoke 通过后
  默认 on / **下个 release 必须 on + 删 flag**（安全链路不可永久 feature gated）
- 🔄 总工时 4.5 h → 6 h（更真实）

**v2 决议请求**（5 个新问题）：
1. v2 整体是否 approve？
2. `OutputPolicy` 7 variant 是否覆盖？需要加 `LimitedForward`（rate limit）吗？
3. `OutputSafetyState` 上 state topic 是否走 `00_design.md` §3.4 schema 评审？
4. M5 行为切换点是否拆两 commit（"flag on" + "删 legacy"）？
5. 真机最低门槛：feetech_poke 读 `Torque_Enable=0` + 物理推夹爪确认 —— 同意 / 加项？

详 `docs/designs/task-30_state_machine_adapter_link.md`（v2，~860 行）。

### v3 修订（2026-04-25 user 二轮 review 后）

User v2 review 给出 7 主问题 + 3 次问题，全部接受 + 重写覆盖文档。718 行
→ 744 行。**关键诚实修正**：v2 把 hardware IO 从 `s0_rt_thread` 移到普通线程
但**声称不改设计**——与 `docs/00_design.md:114` 冻结条款冲突。事实上 Feetech
串口 RTT ms 级**从来不是 HardRT**，但代码（`feetech/src/adapter.rs:202`）仍
标 `RtClass::HardRT`（注释里早就承认是"目标分类"）。v3 把这件事**正式化**为
`task-30-pre-1`。

**v3 新增 3 个前置 task**（必须 task-30 主体 M1-M5 之前完成）：

- **pre-1**（30 min + 评审）：Feetech 重分类 `RtClass::SerialRT`（新增 enum
  variant + 改 feetech adapter `RtClass::HardRT → SerialRT` + 走 CHANGELOG
  设计修订评审 + 加 `10_verification.md` SerialRT baseline 行）
- **pre-2**（1.5 h + 评审）：state schema 补字段（`mode/control_link/output
  _mode/safety_state` 全部加 `Unknown` variant 兼容旧 writer）+ build_helpers
  + pub_thread 真发字段 + robotctl 展示 + schema_compat 测试 + state_machine
  转换表加 `* → Fault(SafetyStopFailed)`（设计修订级 §1.4 / §3.4）
- **pre-3a/b**（45 min）：`RobotError::Unsupported(&'static str)` variant +
  adapter_io_thread 用 `RobotClock` 替代 `Instant::now`（业务代码禁用 §业务
  代码禁用 API）

**v3 主要 v2 修订**：

- ➕ §1.2 / §1.3 / §1.4 v3 新章节：架构修订级问题陈述（Feetech 不是 HardRT；
  schema 与冻结设计不一致）
- 🔄 §5.2 `AdapterCtrlClient` 重写**非阻塞**：v2 用 bounded(1) send 会阻塞
  ipc_thread → 心跳/watchdog/EStop 全卡死。v3 方案 A `pending op slot`（异步
  + state_machine 下次 tick poll resp）；**采用 A**
- 🔄 §5.4 server loop emergency `select!` arm 改**立即 dispatch** —— v2 写
  `recv → _ => loop` 会消费但不处理，丢 EStopLatch 这类语义消息
- 🔄 §5.6 `tick_hardware` `Velocity` 真闭环或 `Unsupported`：v2 set_mode 后
  仍调 `write_command`，但 feetech 对非 Position 静默 no-op = 假闭环。v3：
  `state.current_mode_supports_write` 标位 + 不支持 → `FaultCode::Adapter
  UnsupportedMode` 进 Fault
- 🔄 §6.3 state_machine 转换表加 `* → Fault(SafetyStopFailed)` 全状态接受 ——
  v2 只 `Enabled.* → Fault`，EStop 已 Safe 时再来 SafetyStopFailed 会被拒收；
  这是 §1.4 冻结条款修订，必须走评审
- 🔄 §6.4 `OutputSafetyState` 与 `S0State` **正交独立**（不合并到 Fault），
  state topic 同时暴露两字段（意图 vs 实测）
- 🔄 §7.2 SIGKILL 改写：**不是 task-30 验收项，是 blocking gap**。`docs/10_
  verification.md:102` 冻结 SLA "kill -9 → fail-safe < 500 ms" 留 `task-30-
  post`：servo Goal_Position watchdog 标定 + 硬件继电器
- ➕ §8 测试加 5+ 项：非阻塞 client / state_machine hook 不卡心跳 / SafetyStop
  Failed 全状态接受 / EStopLatch 不丢 / Velocity Unsupported / state schema
  字段全 / pub_thread 真发字段 / robotctl 显示 / **SIGKILL 不通过**（known issue）
- 🔄 §9 迁移加 pre-1/2/3 前置；M1-M5 工作量 6 h → 6.75 h
- 🔄 §11 7 个 v3 决议请求（取代 v2 5 问题）

**v3 7 个决议请求**：

1. v3 整体（pre-1/2/3 + M1-M5 + SIGKILL blocking gap）是否 approve？
2. pre-1 Feetech `RtClass::SerialRT` 走 §4.6 设计修订评审？还是用现有 `SoftRT`
   不新增 variant？
3. pre-2 schema 字段全部加 `Unknown` variant 兼容旧 writer？
4. §5.2 方案 A（pending op slot 异步） vs 方案 B（`send_timeout(50ms)` 紧上限）？
5. `* → Fault(SafetyStopFailed)` 修订是否走 §1.4 转换表评审 + dimos 同步回写？
6. §7.2 SIGKILL 留 `task-30-post` 同意？
7. 真机最低门槛 M5 smoke：`feetech_poke read Torque_Enable=0` + `robotctl status`
   显 `safety_state=TorqueOffConfirmed` + 物理推夹爪。同意 / 加项？

详 `docs/designs/task-30_state_machine_adapter_link.md`（v3，744 行）。

### v4 修订（2026-04-25 user 三轮 review 后 —— 架构级方向修正）

User v3 review 提出**架构级方向修正**：v3 把 hardware IO 永久移到 `adapter_io
_thread`，等于把原设计 §1.2 / §1.4 "HardRT adapter 直挂 s0_rt_thread" 给放
弃了。**这是过度修订**。

User 关键洞察：
- 原设计 §1.2 / §1.4 没错；rt_thread 直接调 HardRT adapter **对真 HardRT 仍
  然成立**（EtherCAT / RT-CAN / DMA mmap / 自家 FPGA shared-mem）
- Feetech 的问题不是设计错，是 **adapter 分类错** —— 标 HardRT 但 ms 级 IO
- 解法：**保留原设计 HardRT 路径不动**；**新增第三类** `RtClass::SerialRT`
  与 HardRT / SoftRT 并列；`s0_daemon` 按 `adapter.info().rt_class` dispatch

判定基准：**可证明的最坏延迟 + RT 铁律合规性**，不靠协议名字。EtherCAT 用了
会阻塞库 / 动态分配 / mutex 也不是 HardRT；某些 CAN bounded + no alloc 就是
HardRT。

**v4 主要修订**：

- 三类 executor 并列：`spawn_rt_thread`（HardRT，**原设计不动**）+ 新增
  `spawn_serial_io_thread`（SerialRT）+ `spawn_io_thread`（SoftRT，v0.4）
- v3 "rt_thread 改读 HotShared / 永久数据通路" → **v4 撤销**；rt_thread 保留
  原设计 `read cmd slot → adapter.write/read → write rt_snapshot`
- v3 OutputPolicy / emergency / IoState / 复杂语义 **范围限定到 SerialRT
  executor**；HardRT executor 的 OutputPolicy gate **简化版**（仅"是否调
  write_command" 分支，不做 hold fallback / mode change 等）
- §3 架构图重画：rt_thread + serial_io_thread + io_thread 三条并列
- §6 新增 `s0_daemon dispatch by RtClass`：`match adapter.info().rt_class`
  决定 spawn 哪个 executor
- 现有 `mock_hard_rt` 保持 `RtClass::HardRT` 不变，走 rt_thread 路径，v0.2
  测试 + smoke 零回归
- Feetech adapter 改 `RtClass::SerialRT`（pre-1），走 serial_io_thread 路径
- 风险加 R9（HardRT executor 简化版 OutputPolicy 语义边界）+ R10（dispatch
  by RtClass 错配的安全风险 + adapter 实现方分类纪律）

**v4 vs v3 本质区别**：
- v3：把 hot path 移走 → rt_thread 永久变数据通路 → **否定原设计**
- v4：**新加一条平行路径** → rt_thread 仍是 HardRT 真接入点 → **原设计完全保留**

文档 744 → 802 行（v4 重画 §3 架构图 + §6 dispatch + §10 R9/R10 + §0 v3→v4
修订表 + §12 7 个新决议）。

**v4 7 个决议请求**：

1. v4 整体（保 HardRT 路径 + 新 SerialRT executor + dispatch by RtClass）approve？
2. `RtClass::SerialRT` 走 §4.6 设计修订 + dimos 同步回写？
3. HardRT executor OutputPolicy gate **简化版** 接受（v0.4 真 HardRT adapter
   需要 hold fallback 等再升级 rt_thread）？
4. `s0_daemon` dispatch 走启动期 match `adapter.info().rt_class` 还是编译期
   trait method（`fn spawn_executor`）？
5. v0.2 范围内 `RtClass::SoftRT` 在 s0_daemon 走 `Err(Unsupported)` 不实施
   接受？
6. M5 行为切换点拆两 commit（flag on + 删 legacy）？
7. 真机最低门槛 M5：mock_hard_rt 走 rt_thread 不回归（task 29 ±17° 双夹爪
   重跑） + feetech 走 serial_io_thread（feetech_poke Torque_Enable=0 +
   robotctl safety_state=TorqueOffConfirmed + 物理推夹爪）。同意 / 加项？

详 `docs/designs/task-30_state_machine_adapter_link.md`（v4，~800 行）。

### v5 修订（2026-04-25 user 四轮 review 后）

User v4 review 给出 6 项 critical 修订，全部接受：

1. **HardRT 安全动作不能只靠"不调 write_command"** —— 与 §0 / §1.4 :149
   "cut output / torque disable" 冲突（"不写"≠"断扭"）。**新增**
   `ExecutorCtrlClient` trait 统一抽象：state_machine 只表达意图，
   HardRT impl 用 atomic action slot（rt_thread tick 内 RT-safe 调
   adapter）+ SerialRT impl 用 emergency / normal channel
2. v4 §4 自相矛盾（"仅 SerialRT" vs "HardRT 也读 policy"）→ v5 重写
   "OutputPolicy 是**所有 motor executor 的统一门控**；执行动作因 executor
   类型不同"
3. RT 路径 `ArcSwap<OutputPolicy>` 有 hazard pointer 复杂度 → 改 **`Atomic
   U8 + #[repr(u8)]`**：`OutputPolicy` 8 variant 单字节够用；RT 读 < 1ns 无
   alloc
4. Feetech `enable(false)` 13 motor × 3 retry × 150ms = 最坏 5.85s 不适合
   emergency → **新增 `safety_stop_fast()`** 走 sync_write broadcast
   (id=0xFE, ~1-2ms)；保留 `enable(false)` 给普通路径
5. SIGKILL 列 blocking gap 但 verification.md:102 仍是 v0.2 SLA → **明确**
   task-30 完成 **≠** v0.2 safety exit criteria 通过；只完成"graceful 软件
   断扭"；硬件 fail-safe 留 task-30-post
6. HardRT `ControlMode::Stop` 合约缺失 → **新增 `RtSafeCtrl` 子 trait**
   （HardRT adapter 必实现）+ `ManipulatorAdapter::write_command(mode=Stop)`
   合约：HardRT adapter 必须实现为 RT-safe cut output，**不**得 no-op

**v5 新增**：

- §4 `ExecutorCtrlClient` trait（4 方法：`set_policy` / `safety_stop` /
  `request_normal` / `poll_last_normal_resp`）；HardRT impl 用 atomic action
  slot；SerialRT impl 用 emergency / normal channel；state_machine `Box<dyn>`
  持有，不感知底层
- §5.1 `OutputPolicy` AtomicU8 + `repr(u8)` 实现
- §5.2 OutputPolicy 在两类 executor 的执行表（HardRT 简化数据 gate；SerialRT
  完整语义）
- §5.3 `RtSafeCtrl` 子 trait（HardRT adapter 必实现 `safety_stop_rt` /
  `enable_rt` / `set_mode_rt`，且 `write_command(Stop)` 必须 RT-safe cut
  output）
- §7 `FeetechAdapter::safety_stop_fast()` 详细设计（sync_write broadcast,
  1-2ms vs 5.85s 慢路径）
- §9 SIGKILL 措辞调整：task-30 完成不等于 v0.2 SLA 通过
- §10 测试加 5+ 项：OutputPolicy AtomicU8 / HardRtCtrlClient action_slot /
  feetech safety_stop_fast 时延 / RtSafeCtrl 合约 / 多 executor 并存
- §11 M1-M5 拆为 M1-M6（加 `RtSafeCtrl` + `safety_stop_fast` + ExecutorCtrl
  Client 三个独立 commit）；总 ~8 h（v4 估 6.75 h，更真实）
- §12 风险加 R11（RtSafeCtrl 合约违约）+ R12（safety_stop_fast 不等 ACK）

**v5 8 个决议请求**：

1. v5 整体（ExecutorCtrlClient + AtomicU8 + RtSafeCtrl + safety_stop_fast）
   approve？
2. ExecutorCtrlClient 4 方法是否覆盖完整？要加 clear_pending / cancel 吗？
3. OutputPolicy 用 AtomicU8 不用 ArcSwap 同意？
4. RtSafeCtrl 子 trait 强制 HardRT adapter 实现接受？
5. FeetechAdapter::safety_stop_fast (sync_write broadcast id=0xFE + 不等 ACK
   + 1-2ms) 设计同意？
6. verification.md:102 状态改 "⏳ blocking gap (软件不可解；待 task-30-post)"
   同意？task-30 实施后此项**仍 ⏳**。
7. M5 行为切换点拆两 commit（flag on + 删 legacy）？
8. 真机最低门槛 M6 smoke 加新项：mock_s1 发 ControlMode::Stop → feetech_poke
   在 mock_s1 跑时 read Torque_Enable=0（验证 Stop 路径走通）。同意 / 加项？

详 `docs/designs/task-30_state_machine_adapter_link.md`（v5，~700 行）。

---

## 2026-04-25 · Step 8 EStop latch 双文件 + 重启恢复（**状态：登记 / 实施完成**）

### 涉及小节

- `docs/01_implementation_steps.md:1366` Step 8 `EStop latch 文件双写 + 重启
  恢复 (fsync 双文件、读取校验、持久化测试)`
- `docs/00_design.md` §6.6 / §6.7 / §1394 风险表 "fsync 双写 + 备份文件"
- `docs/10_verification.md:101-102` Step 8 SLA：EStop latch 持久化必过
- 路线图步骤 6（C task）

### 现状识别

`crates/robot-s0/src/estop_latch.rs` 已存在（task 13 commit `de6ffc6`）：tmp +
fsync + rename + parent dir fsync + 11 单测。但**只写一份文件**，与设计原文
"fsync **双写** + 备份文件"差一份 backup。Step 8 正式补完。

### 修改内容

- `crates/robot-s0/src/estop_latch.rs`：
  - **`save_estop_latch`** 改为双文件：先写 primary 再写 backup（`<path>.bak`），
    每份独立走 tmp + fsync + rename + dir fsync。primary 写完 crash → primary
    新值 + backup 旧值（不丢更新，因为 primary 是权威）；primary 失败 → 直接
    报错（**不**回退到只写 backup）。
  - **`load_estop_latch`** 加 fallback + 自愈：
    - primary 合法 → 用之
    - primary `NotFound` → 试 backup → 拿到合法值后**回写 primary**（自愈）→
      backup 也 `NotFound` → return `false`（首次启动）
    - primary `InvalidData`（损坏）→ 试 backup → 合法 → 回写 primary 自愈；
      backup 也损坏 → return `InvalidData`（**不**默认 false，调用方按 Fault 处理）
  - **`clear_estop_latch`** 同时删 primary + backup（任一 NotFound 视为成功）。
  - 新增 `pub fn backup_path(&Path) -> PathBuf`：派生 `<primary>.bak` 路径。
  - 抽出私有 `read_single` / `save_single_atomic` 共享逻辑。
  - **新增 12 单测**（原 11 + 新 12 = 23 共）：backup_path 派生 / 双文件写入 /
    primary 缺失 fallback / primary 损坏 fallback / 两份都缺失 / 两份都损坏
    （Err InvalidData 不默认 false）/ backup 损坏走 primary / clear 双删 /
    save 覆盖双文件 / 部分损坏不主动修 backup（设计选择：primary 权威）/ restart
    持久化往返。
- `docs/10_verification.md:101`：Step 8 EStop latch 持久化往返 `⏳ → ✅`；
  新增"双文件 fail-over"行 ✅；同时把 `kill -9 → fail-safe < 500 ms`
  状态改 **`⏳ blocking gap`** + 标 task-30-post（task-30 v3 §7.2 已对齐）。
- 不动 `s0_daemon.rs`：`load_estop_latch` / `save_estop_latch` 公开 API 签名
  不变，调用方零改动；backup 路径自动派生。

### 关键决策

- **API 兼容**：保留单 path 参数；backup 隐式派生 `<primary>.bak`。这样
  `s0_daemon` 既有路径计算（`/var/lib/robot/<id>/estop_latch`）/ CLI flag
  `--latch-path` / 单测都不用改。
- **primary 权威**：load 时 primary 合法即用，不主动检查 backup 是否对齐。
  backup 仅在 primary 失败时兜底。设计选择是"备份文件"而非"双副本同步"。
- **两份都损坏 = Fault**：不默认 `false`，避免"系统级故障被掩盖成首次启动"。
  调用方（`s0_daemon`）应进 Fault 状态。
- **`kill -9 → fail-safe` 状态明确为 blocking gap**：与 task-30 v3 §7.2 对齐。
  软件层无法保证 SIGKILL 后的 fail-safe；需要 servo timeout 标定 + 硬件继电器
  （task-30-post）。Step 8 不闭环此项。

### 验证

- `cargo fmt --all --check` ✅
- `cargo clippy --all --all-targets -- -D warnings` ✅（含 `--features real-hw`）
- `cargo test --all` ✅（estop_latch 23 单测；workspace 共 393 pass / 0 fail）
- 单测覆盖 §6.7 "fsync 双写 + 备份文件"全部失效模式：primary 损坏 / 缺失 /
  两份都坏 / 部分损坏 / clear 双删 / restart 往返

### 未做（留 task-30 / Step 9）

- **EStop topic → state_machine 真触发 `safety_stop()`**（adapter 真断扭）
  → task-30 M5
- **`ClearEStop` RPC 端点**（设计 §6.7 要求 maintainer+）→ Step 7+ RPC 网关
- **物理 estop button → latched < 5 ms**（`docs/10_verification.md:100`）→ Step 10 真硬件
- **kill -9 s0 → fail-safe < 500 ms**（`docs/10_verification.md:102`）→ task-30-post
  （servo timeout 标定 + 硬件继电器）

### 审阅

- 提议：Claude Code（路线图步骤 6 "C Step 8 EStop latch"）
- 实施：本 commit
- 复审触发：
  - task-30 实施时 `EStopLatch` emergency 路径调 `save_estop_latch(true)` →
    一并验证双文件路径在真机 SIGTERM 路径下能落盘
  - bus2 / 13-motor 多 hardware_id 时 latch 路径计算 → `s0_daemon` 路径策略可能
    需要扩
