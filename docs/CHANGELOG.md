# CHANGELOG

> 本文件记录**偏离 `docs/00_design.md` 冻结语义**的讨论与决定。
> `00_design.md` 本身不再修改；此文件是"已冻结文档之外的例外登记簿"。
> 每条记录：日期 + 偏离的设计文档小节 + 理由 + 约束 + 影响面 + 审阅人。

---

## 2026-04-21 · 工具链：使用 nightly

### 偏离

`docs/00_design.md` §9《构建与工具链》原话：

> Rust **stable**（尽量不用 nightly）。

v0.2 开工决定：**本项目开发 / CI 默认使用 nightly toolchain**（`rust-toolchain.toml` pin `channel = "nightly"`）。

### 理由

1. 全局 `~/.claude/CLAUDE.md` §5 已把 nightly 作为用户个人默认 Rust 通道（与 `mimalloc` 全局分配器配套）；本项目单独维护 stable 通道会增加切换成本。
2. `00_design.md` §9 措辞为 "尽量不用 nightly"（soft constraint），并非 "严禁 nightly"；此解读在原文字面内。
3. nightly 只决定**编译器版本**，并不强制引入 `mimalloc` 或任何 unstable 特性 —— `00_design.md` §0 Allocator 策略（S0/S1 用系统默认 + RT 禁 alloc）不因 nightly 工具链而失效。

### 约束（保留随时切回 stable 的能力）

1. **核心 crate 禁用 `#![feature(...)]` unstable feature**：
   - `robot-core` / `robot-msg` / `robot-transport` / `robot-s0` / `robot-s1`
   - 任何一个 `.rs` 出现 `#![feature(` 或 `#[cfg_attr(..., feature(...))]` 引入未稳定 feature，视同破坏约束，CI lint 拦截（grep + clippy `disallowed_attributes` 策略；Step 1 之后补）。
2. **MSRV 保持近期 stable**：`workspace.package.rust-version = "1.82"`（或后续升级到当时最新稳定版），确保任何核心 crate 在 stable 通道都能编译通过。
3. **工具类 crate 可使用 nightly-only feature**（`robot-cli` / `robot-viz` / `robot-rpc` / `robot-s2-bridge` / `robot-tests` / `robot-recorder`），但须在该 crate 顶部注释 + README 列出所用 unstable feature 清单，便于将来迁移。
4. **S0 / S1 RT 约束不变**：`panic = "abort"`、`mlockall`、禁 heap alloc、禁 `Arc::clone/drop` 等铁律（`00_design.md` §0、§4.9）全部保留。
5. **S0 / S1 不得依赖 `mimalloc`**（即便全局偏好启用），该约束来自 `00_design.md` §0 Allocator 策略，未偏离。
6. **CI 回退演练**：每次打 `v*` tag 前，在 stable 通道跑一轮核心 crate 的 `cargo build + cargo test`（不过 `clippy -D warnings`），作为"必要时回切 stable"的健康证据。失败需先修复再发版。

### 影响面

| 文件 | 调整 |
|------|------|
| `rust-toolchain.toml` | `channel = "nightly"`（Step 0 创建） |
| `CLAUDE.md`（项目） | 《Rust 规范》首条由 "stable only" 改为 "默认 nightly + 核心 crate 禁 unstable feature"；workspace 布局注释同步改 |
| `docs/02_start_here.md` | 《硬性约束》第 4 条同步修订 |
| `docs/01_implementation_steps.md` | 《不要做的事》第 6 条由 "不用 nightly Rust" 改为 "核心 crate 不启用 nightly unstable feature" |
| `docs/00_design.md` | **不改**（冻结文档原则） |

### 审阅

- 提议：Claude Code（Opus 4.7 / 1M context）
- 决定：用户 loosqk（2026-04-21 本 session 内明确指令 "继续用 nightly，改一下文档"）
- 复审触发条件：
  - 若核心 crate 因 iceoryx2 / flatbuffers 某版本要求 stable-only feature → 立即回切 stable 并废止本条偏离。
  - 若 nightly 在连续 3 周内出现破坏核心 crate 编译的回归 → 降级为 "CI pin 具体 nightly 日期" 或回切 stable。

---
