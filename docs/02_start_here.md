# 02 · Claude Code 上手指南

> 给在 `/Users/loosqk/repo/rust/robot-system/` 开新 session 的 Claude Code 读。

---

## 你接手的是什么

`robot-core`（工作代号 `robot-system`）—— 一个 **Rust 通用机器人运行时**，三层架构（S0 反射 / S1 快反射 / S2 慎思），本机通信走 iceoryx2 + FlatBuffers，对外走 gRPC。

目标：替换上游 dimos 项目（Python 框架）的运行时部分，同时保持可跨机器人复用（XLeRobot、Piper、移动底盘、未来四足 / 人形）。

用户是**追求完美的 Rust / C++ 程序员**，见 `~/.claude/CLAUDE.md`（中文交流、commit 带 emoji、commit 前必 fmt + clippy 零警告、不加 Co-Authored-By）。

---

## 当前状态

```
设计版本:     v0.1.2 已冻结（docs/00_design.md）
实施阶段:     v0.2 最小闭环
当前步骤:     Step 0 未开始（见 docs/01_implementation_steps.md）
平台:         Step 0-5 macOS 可跑；Step 6+ 需 Linux（Orin 或 Linux 开发机）
```

---

## 文档阅读顺序

**首次进入 session 必读（按顺序）**：

1. `CLAUDE.md`（本项目工作约定，包括禁用 API 清单）
2. 本文档 `docs/02_start_here.md`
3. `docs/00_design.md`（冻结设计，**权威**）
4. `docs/01_implementation_steps.md`（实施步骤，**执行依据**）

**用户通用偏好**：`~/.claude/CLAUDE.md`（已由 Claude Code 自动加载）

**冲突裁决**：`00_design.md` > `CLAUDE.md` > 全局 CLAUDE.md > 本文档。

---

## 硬性约束（破一条都不行）

1. **不改 `docs/00_design.md`**。架构已冻结。发现语义问题 → 写 `docs/CHANGELOG.md` 讨论，不直接改设计。
2. **不跳步**。`docs/01_implementation_steps.md` 的 10 步顺序强制。
3. **commit 前零警告**：`cargo fmt --all --check && cargo clippy --all --all-targets -- -D warnings`。
4. **Rust nightly 是硬要求，严格不用 stable**（对应 `docs/00_design.md` §9）。**核心 crate**（`robot-core` / `robot-msg` / `robot-transport` / `robot-s0` / `robot-s1`）**原则上不写 `#![feature(...)]`**，目的是降低 nightly 升级回归风险（不是为了留 stable 后门）；确需启用必须 crate 顶部注释 + review。
5. **业务代码禁用 API**：
   - `std::time::Instant::now` / `SystemTime::now` / `tokio::time::Instant::now` → 走 `robot-core::time::RobotClock`
   - `google.protobuf.Any` 作为 RPC payload → 用 `CommandMeta` + 强类型 Command
6. **RT thread 禁令**（设计文档 §0）：禁 heap alloc / `Arc::clone`、`Arc::drop` / 文件 IO / 网络 IO / `panic!` / `format!` / 带 format arg 的 `tracing::info!`。
7. **commit message 不加 Co-Authored-By**（全局 CLAUDE.md §4）。

---

## 第一个任务：Step 0

打开 session 后没有特殊指示，直接按 `docs/01_implementation_steps.md` Step 0 执行：

1. 写 `Cargo.toml`（workspace 根，**无** `[package]`，列出未来所有 crate 但此刻只 include `robot-core`）
2. 写 `rust-toolchain.toml`（pin `channel = "nightly"`；可进一步 pin 具体日期 `nightly-YYYY-MM-DD`）。`Cargo.toml` 设 `edition = "2024"`，**不写 `rust-version`**（自动跟随最新 nightly；严格不用 stable）。
3. 写 `.cargo/config.toml`（sccache rustc-wrapper、target 目录可选）
4. 写 `.gitignore`（`target/`、`.DS_Store`、`*.sccache*`）
5. 建 `crates/robot-core/`，`src/lib.rs` 留空 `// robot-core v0.1.2 frozen design`
6. 跑三件套：`cargo build --all` / `cargo fmt --check` / `cargo clippy -D warnings`
7. `git add -A && git commit -m "✨ feat: 初始化 workspace"`

完成后直接进 Step 1（`robot-core::time`）。

---

## 工作方式

- **少解释、多动手**。用户已经读过设计文档 1500 行，不需要你再复述。
- **先读后写**。修改任何文件前先 `Read` 一次。
- **小步 commit**。一个逻辑单元一个 commit，不要攒一大堆。
- **遇到未决就问**，不私自做架构决策。
- **不主动创建 README.md / 文档** 除非用户明确要求（全局 CLAUDE.md）。
- **保持 CLAUDE.md / 01_implementation_steps.md 新鲜**：每完成一步更新状态；发现文档错漏顺手改。
- **每步结束把 `docs/01_implementation_steps.md` 顶部 "当前位置" 更新**到下一步。

---

## 上游参考（dimos 项目）

`/Users/loosqk/repo/robot/dimos/` 是 XLeRobot 现役控制栈（Python + dimos）。需要硬件信息时查：

| 用途 | 路径 |
|------|------|
| Feetech STS3215 寄存器序列规范 | `docs/06_lerobot_replace_spec.md` |
| 自研 Python STS3215 驱动（移植参考） | `src/xlerobot_dimos/common/feetech_io/` |
| 弧度 ↔ raw / 三轮全向运动学 | `src/xlerobot_dimos/common/units.py` |
| 校准 JSON 示例 | `calibration/` |
| URDF + STL（未来 IK / 3D 渲染） | `assets/xlerobot/` |
| Phase 6 OpenPI + RL 集成（S2 参考） | `docs/07_phase6_openpi_rl.md` |

**这些是参考，不是依赖**。本项目不 import Python 代码，要**移植成 Rust**。

---

## 和 dimos 仓库的关系

- 本项目是**独立 git 仓库**（`/Users/loosqk/repo/rust/robot-system/`），与 dimos 无 git 联系。
- 设计文档源版本在 dimos `docs/09_robot_framework.md`；本项目的 `docs/00_design.md` 是**复制快照**，不反向同步。
- 如果在本项目发现需要改设计：先在本项目 `docs/CHANGELOG.md` 记录，评审通过后由用户决定是否同步回 dimos 侧。
- v0.4 完成后，dimos 项目会切换成调用 robot-system 的二进制 + RPC / iceoryx2 客户端。

---

## 禁止讨论的话题（架构已冻结）

遇到以下话题直接引用 `docs/00_design.md`，不展开：

- iceoryx2 vs eCAL / DDS / Zenoh 机内替代
- FlatBuffers vs Cap'n Proto / protobuf
- gRPC vs ZMQ / HTTP
- S0/S1/S2 重分层或合并
- Python 是否能进 S1 主进程
- SoftRT 是否能挂 `s0_pub_thread`
- `layout_id` 宽度（已定 u64）
- `panic = "abort"` 范围（已定仅 S0/S1）
- `CommandMeta` 是否用 `Any`（已定强类型）

要改 → `docs/CHANGELOG.md` + 用户评审。

---

## 常见陷阱

- 在 `robot-core` 里顺手加 `tokio` / `tracing-subscriber` 依赖 → 破坏"零 runtime 依赖"铁律（§4.2）。
- RT thread 里写 `if debug { tracing::info!(...) }` → 日志 format 参数构造仍分配。
- `ReloadCalibration` 用 `Arc::clone` → 破坏 §4.9 硬约束，必须走 `RtSnapshotReader/Publisher`。
- `layout_id` 被手写成 `u32` → v0.1.2 已升 u64，所有 schema 字段也是 `ulong`。
- 发 RPC 命令不带 `CommandMeta` → 破坏幂等性保证。
- commit message 不带 emoji / 用 Co-Authored-By → 全局 CLAUDE.md §4 硬性规则。

---

## 结束 session 前

- 确认 `cargo fmt + clippy` 零警告
- 确认当前 step 的 "退出标准" 全部达成再宣称完成
- 确认 `docs/01_implementation_steps.md` 的 "当前位置" 行已更新
- 如果还没到下一步自然的 commit 边界，不要留未 commit 的大改
