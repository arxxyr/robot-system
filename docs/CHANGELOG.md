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
