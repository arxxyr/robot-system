# CHANGELOG

> 本文件记录 `docs/00_design.md` 冻结后的语义讨论与决定。
> 两类条目：
> 1. **偏离登记**：允许临时偏离设计文档（冻结原则下）；
> 2. **回写登记**：用户拍板后，通过 v0.1.2 "冻结后文档卫生" 机制（附录 E）修改设计文档正文，同步回写 dimos 侧源版本 `docs/09_robot_framework.md`。
>
> 每条记录：日期 + 涉及小节 + 状态（偏离 / 已回写）+ 理由 + 约束 + 影响面 + 审阅人。

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
- 例外：`libc = "0.2"`（仅 `robot-core::time` 与未来 `robot-core::os`，见 2026-04-21 条）
- 禁止：`iceoryx2 / tonic / pyo3 / tokio / tracing-subscriber / vendor SDK / flatbuffers runtime / serde_json`

#### `robot-msg`（v0.2 Step 2 登记）
- 允许：
  - `flatbuffers = "24"`（与 vendor flatc 24.3.25 对齐；Step 2 schema 反序列化必需）
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
| `docs/01_implementation_steps.md` Step 2 交付 | 显式列出 `flatbuffers = "24"` 并指向本条 |

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
   - 版本 pin `flatbuffers 24.3.25`，与 Rust `flatbuffers = "24"` 对齐
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

3. **依赖白名单扩展到 workspace**：`flatbuffers = "24"` 作为 `robot-msg` 的第一条白名单入库，见本文件 "workspace runtime 依赖白名单机制" 条目

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
