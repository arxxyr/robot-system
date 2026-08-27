# Spec · v0.5 候选 A — operator CLI（`robotctl`）扩 tonic gRPC 7 子命令

> 本 Spec round 1 沿 [`v0_4_robot_kinematics_round2.md`](v0_4_robot_kinematics_round2.md)
> + [`v0_4_scripted_backend.md`](v0_4_scripted_backend.md) 同款模板：
> What / 影响范围 / 风险 / 回滚 / 交付 / AC + Q-CLI 拍板表 / reviewer
> 审查重点 / 不在范围 / 后续 Plan 接班。
>
> CodeMap 锚点：[`team-swe/codemap/v0_5_operator_cli.md`](../codemap/v0_5_operator_cli.md)
> （commit `4714257`；含 §0 修订 + §1-§7 摸底事实 + §8 Q1..Q15 拍板
> 候选选项）。
>
> 写于 2026-05-28；reviewer **已拍板 Q1..Q15**（详 §7 拍板表）+
> 额外锁定 `clear-estop` safety contract（详 AC-3 + §6.5 CLI contract
> 表）。**不**重新开 Q16。
>
> **入口对齐**：v0.5_entry §2.1 / §3.2 / §4.1 / §4.3 / §6 "首次 land
> `crates/robot-cli/` workspace member" 措辞已在 commit `0491f58`
> 修订为"扩展既有 binary"；本 Spec 沿修订后版本。

---

## Round 2 amend 记录（2026-05-28；reviewer round 1 复审后）

reviewer round 1 复审给出 5 阻断 + 2 非阻断修订；本 round 2 amend 全部
吸收（**AC 编号 / Q-CLI 拍板不变**；仅修内部不一致 + 去未拍板依赖）：

| # | 类别 | 修订 | 落点 |
|---|---|---|---|
| 1 | 阻断 | `--token` 语义统一：**`--token` 优先 → 否则 env `ROBOT_TOKEN` → 两者都缺 exit 2**；7 子命令表述同步（撤"--token 必填"措辞）| AC-3 #3 + AC-4 表 + AC-6 |
| 2 | 阻断 | 去未拍板依赖 / 未定义 helper：**不**加 `whoami` / `rand` / `getrandom`；`source` 默认走 `USER`/`USERNAME` env fallback；`idempotency_key` 自动生成复用本次 UUIDv7 = `cli-<request_id>`（撤 `unix_ms()` + `random_hex_8()`）| AC-6 + Q-CLI5 行 |
| 3 | 阻断 | 撤未拍板 `--joints` 参数：`positions` 长度由 `--waypoints-file` JSON 内容决定 + CLI 仅做 ragged 等长检查；关节数与 hardware 匹配交 server 端 backend（mismatch → exit 4）| AC-8 |
| 4 | 阻断 | smoke `stream_events_sigint` 实测信号 `SIGTERM` → **`SIGINT`**（对齐 Q-CLI10 (c) + AC-10 只处理 `ctrl_c()`）| AC-12 phase 7 |
| 5 | 阻断 | smoke `clear_estop_happy` 改名 **`clear_estop_auth_denied_after_local_gate`** + 语义澄清（验"CLI 四道闸通过 → RPC 已发 → server 权限拒绝"，**非** happy path）| AC-3 smoke (1) + AC-12 phase 2 |
| 6 | 非阻断 | dep 数对齐：AC-1"新加 6 条"→"新加 **7** 条"；回滚段"删 5 行 dep"→"删 **7** 行 dep" | AC-1 + 回滚 |
| 7 | 非阻断 | Plan P.0 工作基线 HEAD `0491f58` → **`ee4b47f`**（Spec round 1 land）| §P.0 |

---

## Plan round 1 amend 记录（2026-05-29；reviewer Plan round 1 复审后）

reviewer Plan round 1 **不批准进入 (1/5)**，给出 3 阻断 + 1 非阻断；本
轮吸收。**阻断 1 触及 AC-2 语义** —— 按 reviewer "必须先回修 Spec" 路径
做 **Spec round 3 amend**（非在 Plan 里偷改 AC 语义）；阻断 2/3 是 Plan
层修订：

| # | 类别 | 层级 | 修订 | 落点 |
|---|---|---|---|---|
| P1-1 | 阻断 | **Spec round 3** | regression 等价判据不在 Plan 用"mask volatile ts"改 AC；**回修 Spec 为可达成的精确判据**：`status`/`watch` = **deterministic byte-level diff = 0**（FakeStatePublisher 喂固定 published state；二者输出只回显 published stamp、无自生 clock，robotctl.rs:372-426 → 真 byte 稳定）；`estop` = **exit code + 发布的 EStopEvent body 字段等价**（reason/source/latched）—— estop 输出含自生 wall-clock `stamp_ns`（`SystemClock::now()`，robotctl.rs:335-336/358），**无 byte 稳定产物 → 不做 byte-diff**（不是对 status/watch 的 mask，是对"自带时间戳命令"的诚实契约）| AC-2 + AC-12 phase 11 + reviewer 审查重点 1 + Plan D1 / §P.2.5.d |
| P1-2 | 阻断 | Plan | **撤 clap**：(1/5) 不引 clap；沿既有**手写 parser**（`crates/robot-cli/src/bin/robotctl.rs:96 Args::parse`）append；真要 clap 须回 Spec/Q/AC 扩依赖范围（本轮不做，AC-1 dep 集不含 clap）| §P.2.1.a + §P.1 commit chain |
| P1-3 | 阻断 | Plan | **(1/5) 不落可达 7 子命令 `unimplemented!()` stub**（会让已暴露 CLI 路径 panic / 违 AC-5 exit code 契约）：parser variant 随**各自 impl commit** 落地；(1/5) 只落共享 lib + 纯路由函数(by-name) + 内部测试脚手架；parser 不动 → `robotctl clear-estop` 在 (1/5) = "unknown subcommand" **exit 2**（既有行为 robotctl.rs:110 → :210-213，不 panic / 不违 AC-5）| §交付 + §P.1 + §P.2.1 + §P.2.2/2.3/2.4 |
| P1-4 | 非阻断 | Spec | AC-1 "既有 6 条 dep"（实列 7 个）→ "既有 dep" | AC-1 |

> reviewer 对 D2-D5 方向无异议（仅 D1 需先解决 Spec/Plan 层级）。设计版本
> 仍 v0.1.2 冻结；本 amend 只动 team-swe Spec/Plan 工作文档，**不**触
> `docs/00_design.md` / frozen schema / `rpc_roles.toml`。

---

## What

为 v0.5 候选 A 落地 maintainer 本地 operator CLI 真实接入所需最小可
用闭环：

1. **扩展 `crates/robot-cli/`**（v0.2 Part 2 已 land；含 3 个 iceoryx2
   直连 S0 子命令 `status` / `watch` / `estop`）加 7 个 tonic gRPC
   子命令（**不**重新搭脚手架；既有 3 子命令行为 **0** 改动）：
   - `robotctl clear-estop`（maintainer+；`ClearEStop` RPC；safety-
     critical；§6.5 contract 严格锁）
   - `robotctl clear-error`（maintainer+；`ClearError` RPC）
   - `robotctl get-task-status <action-id>`（CLI 本地要求 token；
     `GetTaskStatus` RPC。**read RPC：服务端当前 unauthenticated、token
     不上 wire** —— "viewer+" 仅未来 metadata auth 目标 / operator UX，
     **非服务端 enforcement**，详 §P.4 #C1）
   - `robotctl cancel-task <action-id>`（operator+；`CancelTask` RPC；
     写 RPC 带 `CommandMeta`，服务端真做 authz）
   - `robotctl stream-events <action-id>`（CLI 本地要求 token；
     `StreamTaskEvents` RPC，server-streaming。**read RPC：服务端当前
     unauthenticated、token 不上 wire**，同 get-task-status，详 §P.4 #C1）
   - `robotctl invoke scripted --waypoints-file <PATH>`（operator+；
     `InvokeTask` kind=Scripted；`ScriptedPayload` fbs build）
   - `robotctl invoke ik --target-x ... --target-y ... --target-z ...`
     （operator+；`InvokeTask` kind=Ik；`IkPayload` fbs build）

2. **`crates/robot-cli/` Cargo.toml 扩 deps**：加 `robot-rpc =
   { workspace = true, features = ["client"] }` + `tonic` + `tokio`
   + `flatbuffers` + `robot-msg`（已有；继续沿用）+ `uuid`（用
   `Uuid::now_v7()` 替代 smoke binary 4 处 `uuidv7_compat()` inline
   实现 —— **仅 robot-cli 用 uuid 直接 API；4 处 smoke `uuidv7_compat
   ()` 不动**，沿 reviewer Q14 (b) 拍板）

3. **入口 async / sync 分流**（reviewer Q2 (b) 拍板）：`fn main()` 顶
   部 parse subcommand → 既有 3 子命令（`status` / `watch` / `estop`）
   走原 sync 路径；7 个新 tonic 子命令走 `tokio::runtime::Builder::
   new_multi_thread().build()?.block_on(...)`

4. **共享 client harness**（`crates/robot-cli/src/lib.rs` 扩，沿
   reviewer Q7 (b) 拍板）：
   - `connect_gateway(addr: &str, timeout: Duration)` 连接 + readiness
   - `build_command_meta(...)` `CommandMeta` 构造（含 token / source
     / idempotency-key / timeout-ms / dry-run / audit-reason 字段）
   - `tonic::Code → ExitCode` 映射表（沿 AC-5 完整表）
   - `--json` 输出 schema 化（snake_case + `<key>_ns` 整数 + enum
     大写；详 AC-7）

5. **`robotctl_smoke` cross-process e2e binary**（reviewer Q9 (a) 拍
   板）：新 `crates/robot-tests/src/bin/robotctl_smoke.rs` + feature
   `robotctl-smoke`；feature-gated；真起 gateway + s1_daemon + 跑 7
   个 CLI 子命令 + 验输出 / 退出码（沿 candidate 1 / 2 / 3 / 4 同款
   spawn subprocess 模板）

具体 CLI contract / 错误码映射 / 输出格式 / safety contract 见 §6
AC + §7 reviewer Q-CLI1..Q-CLI15 拍板表。

---

## 影响范围

### 必改文件（候选 A land 必须）

| 模块 | 改动 |
|---|---|
| `crates/robot-cli/Cargo.toml` | `[dependencies]` 加 `robot-rpc = { workspace = true, features = ["client"] }` + `tonic = { workspace = true }` + `tokio = { workspace = true, features = ["macros", "rt-multi-thread", "signal", "io-util"] }` + `flatbuffers = { workspace = true }` + `uuid = { workspace = true }` + `serde = { workspace = true, features = ["derive"] }` + `serde_json = { workspace = true }`（`--json` 输出 + `--waypoints-file` JSON parse）|
| `crates/robot-cli/src/lib.rs` | 从空 lib 扩 client harness module：`pub mod gateway_client` (connect / disconnect / readiness) + `pub mod meta` (CommandMeta 构造 + token 加载) + `pub mod exit_code` (tonic Code → ExitCode 映射) + `pub mod json_out` (struct + serde 序列化) + `pub mod uuid_v7` (`uuid::Uuid::now_v7().hyphenated().to_string().to_lowercase()` thin wrapper) + lib unit |
| `crates/robot-cli/src/bin/robotctl.rs` | `fn main()` 顶部 subcommand match 扩 7 个新 arm；既有 `Status` / `Watch` / `Estop` arm 走原 sync 路径不动；7 个新 arm 走 `tokio::runtime::Builder` block_on；help / usage 文本同步扩 |
| `crates/robot-tests/Cargo.toml` | 新 `[[bin]] name = "robotctl_smoke" path = "src/bin/robotctl_smoke.rs" required-features = ["robotctl-smoke"]` + `[features] robotctl-smoke = ["dep:robot-rpc", "dep:tonic", "dep:tokio"]`（沿 `scripted-backend-smoke` 同款 feature 守门）|
| `crates/robot-tests/src/bin/robotctl_smoke.rs`（新）| cross-process e2e；沿 `scripted_backend_smoke.rs` / `ik_backend_smoke.rs` 同款 CaseCtx / FakeStatePublisher / SubprocessHandle / bring_up / tear_down inline 复制；通过 `tokio::process::Command::new(args.bin_dir.join("robotctl"))` spawn `robotctl <subcommand> ...` 验输出 + 退出码（**不**直接调 tonic client；CLI 才是 unit-under-test） |

### 可选改动（reviewer Plan round 决定）

| 模块 | 改动 |
|---|---|
| `crates/robot-cli/README.md`（如已无则新；可选）| crate 用途 + 候选 A 扩 tonic 子命令 first release 状态 + 7 个子命令一行摘要 + 跟 既有 3 iceoryx2 子命令并存边界（Q1 (a) 锁定）+ CLI exit code 速查表（AC-5）；reviewer Plan round 决定是否进首版 commit |
| `configs/robotctl_smoke.toml`（新；如 reviewer Plan 决定不复用 `configs/scripted_backend_smoke.toml` 则新建）| 5-DOF XLeRobot 配置；profile `[s1.profile.backends = ["mock_dwell", "hold_pose", "ik", "scripted"]]`（覆盖所有 7 子命令 RPC 路径需要的 backend；沿 ik_backend_smoke + scripted_backend_smoke 同款）|

### 严禁改动（hard boundary）

下列文件 / 路径 / 语义在候选 A **零**改动；任一改动 = 触发 reviewer
round 中止 / 走独立 reviewer round 单独拍板：

- **既有 `robotctl` 3 个 iceoryx2 子命令**（`status` / `watch` /
  `estop`）：
  * 行为不动（继续走 iceoryx2 直连 S0；`estop` 沿 Q1 (a) 拍板保留
    作 offline fallback）
  * 参数集 / 输出 / 退出码不动
  * SIGSEGV-mitigation signal handler（`shutdown_signal_handler`
    直接 `libc::_exit(0)`）路径不动；候选 A 新 7 子命令走 tokio
    runtime 的 `tokio::signal::ctrl_c()` 路径（async-friendly），跟
    既有 sync 路径**并存不冲突**（入口分流 Q2 (b)）

- **`crates/robot-rpc/` lib**：
  * proto schema **0** 改动（6 RPC 全 frozen）；`crates/robot-rpc/
    proto/robot_control.proto` 不动
  * server / gateway / handler / dispatcher **0** 改动；候选 A 仅
    `client` feature consumer
  * `pub mod auth` / `pub mod proto` 等 export 不扩；候选 A 只 import
    现有

- **`crates/robot-msg/` schemas + `BodyMarker`**：
  * 9 fbs schema_id + `0x040B` (ScriptedPayload) + `0x040C` (IkPayload)
    全部不动
  * fbs builder / `build_helpers` 不扩；候选 A inline 复用既有

- **`configs/rpc_roles.toml`**：4 role × 9 capability 矩阵不动；4
  fake token 不动；候选 A 是 consumer

- **`docs/00_design.md`**：§5.2 (CommandMeta / AuthContext) / §5.3
  14 frozen RPC 表 / §5.4 (4 roles + 9 capabilities) / §6.7 5 不变
  量全部不动

- **候选 1 / 2 / 3 / 4 / round 1 / round 2 已 land smoke binary 行为**
  （`scripted_backend_smoke.rs` / `ik_backend_smoke.rs` /
  `pubsub_cleanup_smoke.rs` / `clear_estop_e2e.rs` / `crash_restart
  _smoke.rs` 等）：
  * 4 处 `uuidv7_compat()` inline 实现**不**清理（reviewer Q14 (b)
    拍板：留独立 follow-up；候选 A 不扩范围）
  * 4 处 `build_invoke_task_cmd` / `build_cancel_task_cmd` /
    `build_task_handle_ref_req` / `build_clear_estop` 等 helper
    inline 实现**不**抽公共 crate（沿候选 4 light Research §6
    reviewer 拍板）

- **`crates/robot-cli/src/bin/robotctl.rs` 既有 `Command::{Status,
  Watch, Estop}` 三个 variant + 对应 run path**：
  * 删除任一既有 variant = 触发 hard boundary 阻断；候选 A 是
    `Command::` enum **append** 7 个新 variant 路径

- **`deny.toml`**：baseline `licenses FAILED` 不修（独立 follow-up
  沿 round 1 / round 2 同款节奏）

- **§6.7 5 不变量任一条降级 / 升级**：5 条全保（候选 A 是 client
  端工具；不动 S0 / S1 / gateway server 端任何语义）

---

## 风险等级

| 风险 | 级别 | 缓解 |
|---|---|---|
| 既有 3 个 iceoryx2 子命令行为漂移（main 入口分流改动连带） | 低 | 入口分流在 main fn 顶部一次 if-else；既有 sync 路径**不**进 tokio runtime；smoke binary 沿候选 1 / 2 / 3 / 4 同款 spawn subprocess + 验输出，回归测试覆盖既有 3 子命令 |
| auth token 泄漏（命令行参数 + env / shell history / process arglist） | 中 | `--token <TOKEN>` 是 dev / staging fake token（rpc_roles.toml 已锁；生产部署改真 token store）；env `ROBOT_TOKEN` 是 Unix 推荐方式；CLI **不**把 token 写到任何 stdout / stderr / `--json` 输出；reviewer 拍板"安全敏感行为 v0.5+ 单独 outline"（不存 keychain） |
| `clear-estop` safety contract 误操作（错的 confirmation / 漏 audit-reason） | 高 | reviewer 额外锁定（§6.5 + AC-3）：`--confirmation` 字面值精确（`CONFIRM_CLEAR_ESTOP` 全大写；CLI **不**接受其它任何值）+ `--audit-reason` 必填且非空（白空字符串拒）+ 缺失/不匹配 exit 2；server 端**仍**做 confirmation 校验（CLI 是 first defense，server 是 last defense；沿 A1 既有契约） |
| `idempotency-key` 自动生成跟 retry 语义冲突 | 低 | reviewer Q5 (b) 拍板自动生成 + retry 时用户显式带同 key；首版 stderr 提示一行 `info: auto-generated idempotency_key=<key>; for retry pass --idempotency-key <key>`（AC-7） |
| stream-events SIGINT 误判终态 | 低 | reviewer Q10 (c) 拍板 exit 0 + stderr `warning: stream interrupted before terminal event`（AC-5 + AC-7） |
| `--waypoints-file` JSON parse 出错信息不友好 | 低 | `serde_json` parse error 行号 / 字段名转 stderr + exit 2；AC-12 锁定字段 schema |
| tonic client connect timeout 默认值过短 / 过长 | 低 | 沿 candidate 1-4 同款 `connect_timeout = 2s` + `timeout = max(2s, args.timeout_ms ms)`；本地 loopback gateway 2s 够 |
| `tokio` runtime 启动开销显著影响 sync 子命令 | 极低 | 入口分流（Q2 (b)）保 sync 子命令完全不进 tokio runtime；async 子命令首次启动 ~10ms tokio overhead（loopback gRPC RPC ~5-50ms 自身耗时下属可忽略） |
| 与 dimos Python client 未来冲突（同 6 RPC 用法漂移） | 低 | 候选 4 light Research 拍板 dimos Python SDK defer；候选 A 行为是 wire-format consumer，不锁外部接口承诺；未来 dimos 切换沿同款 6 RPC + CommandMeta 契约，相互不绑 |

---

## 回滚策略

候选 A 实施期间任一阶段失败 / reviewer 拍板撤回：

1. **`crates/robot-cli/` 改动整体 revert**：
   - `Cargo.toml` 改动撤回（删 7 行新增 dep；详 AC-1）
   - `src/lib.rs` 撤回（回到 15 LoC 空 lib）
   - `src/bin/robotctl.rs` 撤回（回到 426 LoC 3 子命令版）
   - 既有 3 个 iceoryx2 子命令行为 **0** 影响

2. **`crates/robot-tests/` 新 binary 整体撤回**：
   - `Cargo.toml` 撤回（删 `[[bin]] robotctl_smoke` + `[features]
     robotctl-smoke`）
   - `src/bin/robotctl_smoke.rs` 删除
   - 既有 4 个 e2e smoke binary 行为 **0** 影响

3. **`configs/` 改动撤回**（如有）：
   - `configs/robotctl_smoke.toml` 删除（如新建）
   - `configs/scripted_backend_smoke.toml` / `ik_backend_smoke.toml`
     等既有不动

4. **`Cargo.lock` 改动撤回**：
   - 通过 `cargo update` 或 git revert 自动回退（候选 A 只**新增**
     dep；既有 dep 版本不动）

5. **server / gateway / s1 / s0 / adapters 任何代码 0 改动**：
   - 候选 A 是 client 端工具；server / wire 契约 **0** 触；回滚不影
     响任何 §6.7 不变量 / candidate 1-4 已 land 行为

6. **§6.7 5 不变量 0 影响**：候选 A 不动 EStop latch / fault code /
   fsync 双写 / idempotency cache / systemd watchdog 任何主路径

---

## 目标分支 / 交付形式

- 分支：`master`（沿 v0.4 4 个候选 + light Research 同款；候选 A
  不分长期 feature branch）
- 交付：~5-8 commit + 1 summary commit（Plan 期细化拆段）
  * commit ~1：`crates/robot-cli/Cargo.toml` 扩 dep + 入口 async
    dispatch 脚手架 + lib client harness module + 纯路由函数(by-name)
    （fmt + clippy + 编译过；既有 3 子命令回归过；**不**落任何可达新
    子命令 —— parser 不识别新名 → unknown subcommand exit 2，**无**
    `unimplemented!()` stub，Plan round 1 P1-3）
  * commit ~2..N：新子命令按 RPC 家族分组 land（每子命令的 parser
    识别 + arg 解析 + dispatch arm + 实现 + lib unit **同 commit**，
    保证 master 上任一被 parser 识别的子命令都已完整实现；建议家族
    顺序：clear-estop + clear-error → get-task-status + cancel-task +
    stream-events → invoke scripted + invoke ik；详 §Plan）
  * commit ~8：`robotctl_smoke` binary + feature gate + 7 个子命令
    cross-process e2e
  * summary：`docs/handoffs/v0.5_operator_cli_summary.md`

- 不创建新 workspace member（`crates/robot-cli/` 已存在）
- 不发布到 crates.io / PyPI（沿 v0.5_entry §5 #6 hard boundary）
- 不预生成 dimos Python SDK / wrapper（沿候选 4 light Research §1
  defer）

---

## Acceptance Criteria

### AC-1: `crates/robot-cli/Cargo.toml` 扩 dep 合规

- `[dependencies]` 段新加 **7** 条：`robot-rpc` (with `features =
  ["client"]`) + `tonic` + `tokio` (with `features = ["macros",
  "rt-multi-thread", "signal", "io-util"]`) + `flatbuffers` + `uuid`
  + `serde` + `serde_json`
- **不**新增 `whoami` / `rand` / `getrandom` 直接 dep（round 2 amend #2；
  `source` 走 `std::env`，`idempotency_key` 复用 `uuid` 已生成的
  request_id）；`uuid` 的 `v7` feature 传递引入的 `getrandom` 是既有
  workspace dep 子树，非候选 A 新增直接依赖
- 全部走 `{ workspace = true }`（沿 v0.3 / v0.4 既有约定；不显式
  pin minor 版本）
- 既有 dep（`robot-core` / `robot-logging` / `robot-msg` /
  `robot-transport` / `iceoryx2` / `libc` / `tracing`）**0** 改动
- `[lints] workspace = true` 不动
- `cargo build -p robot-cli --release` 编译过
- `cargo tree -p robot-cli --edges normal,no-proc-macro` 不引入意外
  transitive dep（详 AC-Q）

### AC-2: `robotctl` binary 入口分流 + 既有 3 子命令行为 0 改动

- `fn main()` 顶部 parse subcommand 完成后按 Q2 (b) 拍板：
  * `Command::{Status, Watch, Estop}` 走原 sync 路径（`fn run(args:
    &Args) -> Result<(), i32>` 直接调）
  * `Command::{ClearEStop, ClearError, GetTaskStatus, CancelTask,
    StreamEvents, InvokeScripted, InvokeIk}` 走 `tokio::runtime::
    Builder::new_multi_thread().enable_all().build()?.block_on(
    async_run(&args))`
- 既有 3 子命令 stdout / stderr / exit code **0** 改动；regression
  等价判据**按子命令精确分两类**（Spec round 3 amend / Plan round 1
  P1-1；因 `estop` 输出含自生 wall-clock 无 byte 稳定产物，原"任一既
  有子命令 byte-level diff=0"对 estop 不可达成）：
  * **`status` / `watch`：deterministic byte-level diff = 0** ——
    `robotctl_smoke` 用 `FakeStatePublisher` 喂**固定 published
    state**（固定 `stamp_ns` / `seq` / positions）；二者 `print_
    snapshot` 只回显 published stamp、**无自生 clock 调用**
    （robotctl.rs:372-426）→ stdout 真 byte 稳定，对 golden 字面 diff
    = 0
  * **`estop`：exit code + 发布的 `EStopEvent` body 字段等价** ——
    estop 用 `SystemClock::now()` 生成 `stamp_ns` 并打进 stderr
    （robotctl.rs:335-336 / :358），**无 byte 稳定产物**；regression
    判据 = exit 0（Linux `libc::_exit(0)`）+ 收到的 `EStopEvent`
    body `reason` / `source`(=`"robotctl"`) / `latched`(=`true`) 字
    段等价（`stamp_ns` 是 volatile，不进等价判据）。**这不是对
    status/watch 的 mask，是对"命令自带时间戳"的诚实行为契约**
- SIGSEGV-mitigation signal handler 路径保持（既有 sync 路径不变；
  async 路径用 `tokio::signal::ctrl_c()` 不走 `libc::_exit(0)`）

### AC-3: `clear-estop` 子命令 safety contract（reviewer 额外锁定 + §6.5）

- `robotctl clear-estop` 必须满足下列**全部**条件才发 RPC：
  1. `--confirmation <STR>` 字面值**精确**等于字符串
     `CONFIRM_CLEAR_ESTOP`（全大写；ASCII；**不**接受任何变体如
     `confirm_clear_estop` / `CONFIRM-CLEAR-ESTOP` / 含空白前后缀
     版本等）
  2. `--audit-reason <STR>` 必填；空字符串 / 仅含空白字符的字符
     串 / 缺失参数 **全部**拒绝
  3. token 必须可解析（CLI 层）：**`--token <TOKEN>` 优先，否则
     回退 env `ROBOT_TOKEN`；两者都缺 exit 2 + 不发 RPC**（Q-CLI3
     (a) 锁定；**不**是"--token 必填"）
- 任一条件失败：**CLI 端早拒** → stderr 打具体错误 + exit 2
  （**不**发 RPC，避免 gateway audit log 被无效请求污染）
- 通过 CLI 早拒后再发 RPC；server 端**仍**做 confirmation 校验（沿
  A1 既有契约 `clear_estop.fbs::ClearEStopRequest.confirmation` 字
  段；如 server 端校验拒，client 端透传 stderr + exit 4）
- 错误信息文案要求：
  * 缺 `--confirmation`：`error: --confirmation is required for
    clear-estop (must be literal "CONFIRM_CLEAR_ESTOP")`
  * `--confirmation` 不匹配：`error: --confirmation must be
    exactly "CONFIRM_CLEAR_ESTOP", got: <quoted-value>`
  * 缺 `--audit-reason` 或空：`error: --audit-reason is required
    for clear-estop and must not be empty`
  * 缺 `--token`：`error: --token (or env ROBOT_TOKEN) is required`
- lib unit 覆盖 6 个失败路径（缺/匹配错/空白 confirmation × 缺/
  空 audit-reason 笛卡尔积去重）
- smoke phase 覆盖：(1) `auth_denied_after_local_gate`（**非** happy
  path —— round 2 amend #5）：viewer token + 完整合法 4 参数 → **CLI
  四道闸全过 → RPC 已发出 → server 端 PermissionDenied**（viewer 无
  maintainer cap）→ exit 3（验"CLI 早拒通过 + wire 链路通 + server
  端权限拒绝"全链路）；(2) 缺 confirmation → exit 2；(3) confirmation
  不匹配 → exit 2；(4) 空 audit-reason → exit 2

### AC-4: 7 个 tonic 子命令 RPC 调用 + 错误码透传（CodeMap §3.5）

按 §6.5 CLI contract 表，7 个新子命令各自实现：

| 子命令 | RPC | 必填参数验证 | 服务器返回处理 |
|---|---|---|---|
| `clear-estop` | `ClearEStop` | 见 AC-3 | Ok → exit 0 + stdout `cleared_at_ns=<ns> dry_run=<bool>`；error → exit code per §6.5 + stderr `error: <code>: <message>` |
| `clear-error` | `ClearError` | `--audit-reason` 必填非空；token 可解析（`--token` 或 env `ROBOT_TOKEN`，两者缺 exit 2） | 同上 |
| `get-task-status` | `GetTaskStatus` | `<action-id>` UUIDv7 hex 校验；token 可解析（`--token` 或 env `ROBOT_TOKEN`，两者缺 exit 2） | Ok → exit 0 + 状态行；NotFound → exit 4 |
| `cancel-task` | `CancelTask` | `<action-id>` UUIDv7 hex；`--audit-reason` 必填非空；token 可解析（`--token` 或 env `ROBOT_TOKEN`，两者缺 exit 2） | Ok → exit 0 + `cleared_at_ns=<ns>`；error → 透传 |
| `stream-events` | `StreamTaskEvents` | `<action-id>` UUIDv7 hex；token 可解析（`--token` 或 env `ROBOT_TOKEN`，两者缺 exit 2） | 每帧一行 stdout；终态自动关闭 exit 0；FAILED 终态仍 exit 0；SIGINT → exit 0 + stderr warning (AC-7) |
| `invoke scripted` | `InvokeTask` kind=Scripted | `--waypoints-file <PATH>` 必填存在；JSON 合法（AC-12）；`--audit-reason` 必填；token 可解析（`--token` 或 env `ROBOT_TOKEN`，两者缺 exit 2） | Ok → exit 0 + `action_id=<uuid>`；`--follow` → 加 stream-events 跟到终态；error → 透传 |
| `invoke ik` | `InvokeTask` kind=Ik | `--target-x` / `--target-y` / `--target-z` 必填 f64 解析过；`--target-qx/qy/qz/qw` 可选默认 `0 0 0 1`；`--audit-reason` 必填；token 可解析（`--token` 或 env `ROBOT_TOKEN`，两者缺 exit 2） | 同 invoke scripted |

### AC-5: `tonic::Code` → CLI exit code 完整映射表（Q6 (a) 拍板 + Q15 (a) Spec 必拍透）

```
+----------+-----------------------------+----------------------+
| exit code| 来源                        | 语义                 |
+----------+-----------------------------+----------------------+
| 0        | Ok                          | 正常完成             |
| 1        | (内部 fatal)                | panic / 不应到达     |
| 2        | CLI args 错（含 AC-3 safety contract 早拒）| 用户输入错 |
| 3        | tonic::Code::Unauthenticated| token 无效           |
|          | tonic::Code::PermissionDenied| role 无权限         |
| 4        | tonic::Code::FailedPrecondition| server 业务错     |
|          | tonic::Code::NotFound       | action_id / latch 不存在 |
|          | tonic::Code::AlreadyExists  | idempotency dup      |
|          | tonic::Code::InvalidArgument| (server 端 fbs decode 错) |
|          | tonic::Code::Unimplemented  | NoBackendForKind     |
| 5        | tonic::Code::Unavailable    | gateway 未起 / shutting down |
|          | tonic 连接失败 (connect refused / timeout) | network |
| 6        | tonic::Code::DeadlineExceeded| client timeout-ms 到 |
| 7        | tonic::Code::Aborted        | (B4 暂无路径；保留) |
|          | tonic::Code::Cancelled      | (B4 暂无路径；保留) |
| 8        | tonic::Code::Internal       | server 端兜底 internal |
|          | tonic::Code::Unknown        | server 端未分类       |
| 9        | tonic::Code::ResourceExhausted| (B4 暂无路径；保留) |
|          | tonic::Code::OutOfRange     | (B4 暂无路径；保留)   |
| 10       | tonic::Code::DataLoss       | (B4 暂无路径；保留)   |
+----------+-----------------------------+----------------------+
```

- lib `pub fn map_tonic_code_to_exit(code: tonic::Code) ->
  ExitCode`：exhaustive match；不留 `_` 兜底（新 tonic Code variant
  让编译 fail 强制更新）
- stderr 文案：`error: <CODE>: <message>`（`<CODE>` 是 tonic Code
  名字大写，如 `PERMISSION_DENIED`；`<message>` 是 server 端
  `status.message()` 字符串透传）
- lib unit 覆盖每个 exit code 路径

### AC-6: CommandMeta 构造规则 + auth token 处理

- `lib::meta::build_command_meta(...)` 构造逻辑：
  ```rust
  // round 2 amend #2：request_id 先生成一次，idempotency_key 自动
  // 生成时复用它 —— 不引 rand / getrandom 直接 dep。
  let request_id = uuid::Uuid::now_v7().hyphenated().to_string();
  //   ^ uuid（既有 workspace dep）；hyphenated 默认 lowercase
  let token = resolve_token(&args);  // --token 优先 → 否则 env ROBOT_TOKEN
  //   ^ 调用前已 CLI 早拒：两者都缺 → exit 2（见下）
  CommandMeta {
      request_id: request_id.clone(),  // 每次调用都新生成
      source: args.source.clone().unwrap_or_else(default_cli_source),
      //   default_cli_source() = format!("cli.{}", USER/USERNAME/"unknown")
      auth: Some(AuthContext {
          token,                            // 已解析非空
          mtls_fingerprint: String::new(),  // 占位；mTLS v0.5+
      }),
      timeout_ms: args.timeout_ms,  // 默认 2000；--timeout-ms 覆盖
      idempotency_key: args.idempotency_key.clone()
          .unwrap_or_else(|| format!("cli-{request_id}")),
      //   ^ Q-CLI5 (b) 自动生成；round 2 amend 复用 request_id（不再 rand）
      dry_run: args.dry_run,  // flag；默认 false
      audit_reason: args.audit_reason.clone().unwrap_or_default(),
      // (CLI 早拒：5 个写 RPC 必填非空；2 个读 RPC 不必填)
  }
  ```
- `source` 默认值（`default_cli_source()`；**不**引 `whoami` dep —
  round 2 amend #2）：`format!("cli.{}", user)`，其中 `user =
  std::env::var("USER").or_else(|_| std::env::var("USERNAME"))
  .unwrap_or_else(|_| "unknown".into())`（Linux 用 `USER`，Windows 用
  `USERNAME`，都缺回退 `"unknown"`）；`--source <STR>` 覆盖
- `idempotency_key` 自动生成时**必须** stderr 提示一行：
  `info: auto-generated idempotency_key=<key>; for retry pass
  --idempotency-key <key>`（Q5 (b) 锁定）
- `--token` 缺失：先查 env `ROBOT_TOKEN`；env 也缺 exit 2 + stderr
  `error: --token (or env ROBOT_TOKEN) is required`（Q3 (a) 锁定）
- `request_id` 用 `uuid::Uuid::now_v7().hyphenated().to_string()`
  生成（`hyphenated` 默认 lowercase，无需再 `.to_lowercase()`）；
  **每次 `build_command_meta` 调用生成一次并复用**（idempotency_key
  自动生成时拼 `cli-<request_id>`）；**不**自己写 `uuidv7_compat()`
  inline（Q-CLI14 (b) 锁定 robot-cli 不复制 smoke 实现；用 `uuid`
  crate 直接 API）

### AC-7: 输出格式（默认 human-readable + `--json` 切换）

按 Q8 拍板（新 7 子命令支持 `--json`；既有 3 子命令**不**改）：

- **默认（human-readable）**：
  * `clear-estop` Ok：`cleared_at_ns=1234567890 dry_run=false`
  * `clear-error` Ok：`cleared_at_ns=1234567890 dry_run=false`
  * `get-task-status` Ok（EXECUTING）：`state=EXECUTING progress=0.45
    action_id=<uuid>`
  * `get-task-status` Ok（COMPLETED Success）：`state=COMPLETED
    outcome=Success action_id=<uuid>`
  * `get-task-status` Ok（FAILED）：`state=FAILED failure=<kind>
    reason="<reason>" action_id=<uuid>`
  * `cancel-task` Ok：`cleared_at_ns=1234567890`
  * `stream-events` 每帧一行：`at_ns=1234567890 event=STARTED` /
    `at_ns=... event=PROGRESS progress=0.45` / `at_ns=...
    event=COMPLETED outcome=Success`；SIGINT 早退打 stderr
    `warning: stream interrupted before terminal event` (Q10 (c))
  * `invoke scripted` / `invoke ik` Ok（无 `--follow`）：
    `action_id=<uuid>`
  * `invoke scripted` / `invoke ik` Ok（`--follow`）：先打
    `action_id=<uuid>` 一行 + 后续 stream-events 同款每帧一行 + 终
    态 line
- **`--json` 切 JSON 输出**（schema 化）：
  ```json
  // clear-estop / clear-error Ok
  {"cleared_at_ns": 1234567890, "dry_run": false, "request_id": "..."}

  // get-task-status Ok (Executing)
  {"state": "EXECUTING", "progress": 0.45, "action_id": "...", "request_id": "..."}

  // get-task-status Ok (Completed Success)
  {"state": "COMPLETED", "outcome": {"kind": "SUCCESS", "warning": ""}, "action_id": "...", "request_id": "..."}

  // get-task-status Ok (Failed)
  {"state": "FAILED", "failure": {"kind": "ADAPTER_ERROR", "reason": "..."}, "action_id": "...", "request_id": "..."}

  // cancel-task Ok
  {"cleared_at_ns": 1234567890, "request_id": "..."}

  // stream-events: 一行一个 JSON object（NDJSON / JSON Lines）
  {"at_ns": 1234567890, "event": "STARTED", "action_id": "...", "request_id": "..."}
  {"at_ns": 1234567900, "event": "PROGRESS", "progress": 0.45, "action_id": "...", "request_id": "..."}
  {"at_ns": 1234568000, "event": "COMPLETED", "outcome": {"kind": "SUCCESS", "warning": ""}, "action_id": "...", "request_id": "..."}

  // invoke scripted / invoke ik Ok (no --follow)
  {"action_id": "...", "request_id": "..."}
  ```
- JSON schema 规则：snake_case；timestamps 用 `<key>_ns` 整数（u64
  范围内）；enums 用 ALL_CAPS string；ProgressFloat 用 f32；request
  _id / action_id 用 UUIDv7 hyphenated lowercase string；空字段
  `""` / 默认值不省略（schema-stable）
- stderr 输出（warning / info）**不**进 `--json`；只在 stdout 是
  JSON；stderr 永远 human-readable
- lib unit 覆盖每个子命令 default + `--json` 输出 round-trip（fixed
  TaskStatus / TaskHandle / Ack input → exact byte output）

### AC-8: `invoke scripted` `--waypoints-file` JSON schema（Q13 (a) 拍板）

```json
{
  "waypoints": [
    {
      "positions": [0.1, 0.2, 0.3, 0.4, 0.5],
      "dwell_ms": 100,
      "segment_duration_ms": 500
    },
    {
      "positions": [0.2, 0.3, 0.4, 0.5, 0.6],
      "dwell_ms": 0,
      "segment_duration_ms": 1000
    }
  ]
}
```

- 字段对应 `ScriptedPayload` fbs body 的 `WaypointTarget`
  table（`positions: [f64]` + `dwell_ms: u32` + `segment_duration
  _ms: u32`）
- `positions` 长度**由 `--waypoints-file` JSON 内容决定**（首版
  **不**引入 `--joints` 参数 —— round 2 amend #3；该参数未拍板）：
  CLI 仅做 ragged 检查（所有 waypoint 的 `positions` 必须等长，否则
  exit 2 + stderr `error: waypoint positions length mismatch
  (waypoint[<i>] has <n>, expected <m>)`）；具体关节数与 hardware
  匹配**交 server 端** `ScriptedBackend` 对 `hardware.joints.len()`
  校验（mismatch → `HardwareMismatch` → `FailedPrecondition` →
  exit 4）；CLI 仅透传 JSON 提供的 `positions`（沿 `invoke ik` 同款
  "CLI 透传 + server 校验"哲学）
- JSON parse fail → exit 2 + stderr 行号 / 字段名报错（`serde_json`
  默认错误信息）
- 字段长度 / 类型校验 fail → exit 2 + stderr 详细描述
- 文件不存在 / 不可读 → exit 2 + stderr
- lib unit 覆盖 happy path + 5 个 fail path

### AC-9: `invoke ik` 参数 + IkPayload fbs build（Q11 (b) 拍板默认不 follow）

- 必填参数：`--target-x` / `--target-y` / `--target-z`（f64）+
  `--audit-reason` 非空 + `--token`
- 可选参数：`--target-qx` / `--target-qy` / `--target-qz` /
  `--target-qw`（默认 `0 0 0 1` identity quaternion）+ `--seed-joints
  '<j0>,<j1>,<j2>,<j3>,<j4>'`（默认空：无 seed）+ `--frame <STR>`
  （默认空字符串）+ `--follow` flag（默认 false；Q11 (b)）
- f64 解析 fail → exit 2 + stderr
- quaternion normalization：CLI **不**校验（让 server 端 IkBackend
  `XLeRobotArmIkSolver::solve` 校验；CLI 仅透传 4 f64）
- IkPayload fbs build：沿 `ik_backend_smoke.rs` `build_happy_ik_
  payload(...)` 模式 inline 复制（**不**抽到 `crates/robot-msg/src/
  build_helpers.rs` 公共 crate；Q7 (b) 拍板 helper 在 robot-cli lib
  内部）
- lib unit 覆盖 happy path + invalid f64 fail path

### AC-10: `stream-events` server-streaming 行为（Q10 (c) 拍板）

- `stream-events <action-id>` 启动后立即开 stream；每帧到达即写一行
  stdout（无 buffer；line-buffered）
- 终态事件（COMPLETED / FAILED / CANCELLED）后 stream 自动关闭 →
  exit 0（**含** FAILED；终态是有效结果）
- SIGINT（用户 Ctrl+C）：tokio `select!` `ctrl_c()` → 提前 break
  loop → stderr 一行 `warning: stream interrupted before terminal
  event` → exit 0（Q10 (c) 锁定）
- tonic stream error（非终态 server 端关闭）：stderr `error:
  <CODE>: <message>` → exit per §6.5
- `--timeout-ms <MS>` 整流总时长（**不**是单帧）；到期 stderr
  `error: stream timed out after <MS>ms` → exit 6（DEADLINE_EXCEEDED
  exit code）
- `--json` 模式下 stderr `warning` / `error` 仍是 human-readable
  （AC-7 锁定 stderr 不进 JSON）

### AC-11: action-id UUIDv7 hex 本地校验（早拒）

- 3 个子命令接受 `<action-id>` 位置参数：`get-task-status` /
  `cancel-task` / `stream-events`
- CLI 端用 `robot_rpc::auth::validate_request_id` 本地校验 UUIDv7：
  * 空字符串 → exit 2 + stderr `error: <action-id> must not be
    empty`
  * 非 UUID 格式 → exit 2 + stderr `error: <action-id> is not a
    valid UUID: <parse-error>`
  * 非 v7（version 字段不是 7）→ exit 2 + stderr `error:
    <action-id> must be UUIDv7, got version=<v>`
- 校验通过后才发 RPC（避免 gateway audit log 被无效请求污染）
- 通过本地校验后 server 端仍可能 NotFound（action_id 不存在或已
  过 finished ring）→ exit 4
- lib unit 覆盖 3 个 fail path + 1 happy path

### AC-12: `robotctl_smoke` cross-process e2e（Q9 (a) 拍板）

- 新 `crates/robot-tests/src/bin/robotctl_smoke.rs`（feature
  `robotctl-smoke`；feature-gated）
- 沿 `scripted_backend_smoke.rs` / `ik_backend_smoke.rs` 同款 inline
  helper：CaseCtx / FakeStatePublisher / SubprocessHandle / bring_up
  _pair / tear_down_pair
- 用 `tokio::process::Command::new(args.bin_dir.join("robotctl"))`
  spawn `robotctl <subcommand> ...` 真起 CLI 子进程（**不**直接调
  tonic client；CLI 才是 unit-under-test）
- phase 集合（首版；Plan 期定最终）：
  1. `selftest`：bring-up gateway + s1_daemon + 不调 CLI 子命令；验
     harness 通
  2. `clear_estop_auth_denied_after_local_gate`（round 2 amend #5；
     **非** happy path —— 改名前为 `clear_estop_happy`）：viewer token
     调 `clear-estop --confirmation CONFIRM_CLEAR_ESTOP --audit-reason
     "test"` → **CLI 四道闸已通过 + RPC 已发出 → server 端
     PermissionDenied**（viewer 无 maintainer cap）→ exit 3（验"本地
     gate 通过 + wire 链路通 + server 端权限拒绝"；真 happy path 需
     maintainer token + 已 set latch，留独立拍板/后续 phase）
  3. `clear_estop_safety_contract_violations`：4 个失败 case（缺
     confirmation / confirmation 不匹配 / 缺 audit-reason / 空
     audit-reason）→ 全部 exit 2 + 无 RPC 调用（实测 gateway 日志
     无对应 `clear_estop` invocation）
  4. `get_task_status_happy`：先 `invoke ik`（operator token）拿
     `action_id` → `get-task-status <id>` → exit 0 + state ∈
     {EXECUTING, COMPLETED}（race tolerant）
  5. `cancel_task_happy`：`invoke scripted` long-running → 50ms 后
     `cancel-task <id>` → exit 0；后续 `get-task-status` → state =
     CANCELLED
  6. `stream_events_terminal_completed`：`invoke ik` → `stream-events
     <id>` → 终态 COMPLETED → exit 0；stdout 最后一行含
     `event=COMPLETED`
  7. `stream_events_sigint`：`stream-events` 长 stream 启动 → 50ms
     后给子进程发 **`SIGINT`**（`kill -INT <pid>` / `nix::sys::
     signal::kill(pid, SIGINT)`；**不**用 `SIGTERM` —— round 2 amend
     #4，对齐 Q-CLI10 (c) + AC-10 实现只 `select!` `tokio::signal::
     ctrl_c()`，`SIGTERM` 默认走进程终止不触发 warning 路径）→ exit 0；
     stderr 含 `warning: stream interrupted before terminal event`
  8. `invoke_scripted_happy`：`invoke scripted --waypoints-file
     <tmpfile>` happy JSON → exit 0 + `action_id=<uuid>`
  9. `invoke_ik_happy`：`invoke ik --target-x 0.20 --target-y 0.0
     --target-z 0.10` → exit 0 + `action_id=<uuid>`
  10. `json_output_round_trip`：每个 happy phase 加 `--json` flag →
     stdout 行 `serde_json::from_str` parse 通过 + 字段集 cover
     AC-7 schema
  11. `existing_subcommands_regression`（按 AC-2 精确判据分两类）：
     真起 s0_daemon（mock）+ `FakeStatePublisher` 固定 published
     state →（i）`status` / `watch` stdout 对 golden 字面 **byte-level
     diff = 0**（deterministic）；（ii）`estop` → exit 0 + 收到的
     `EStopEvent` body `reason`/`source`/`latched` 字段等价（`stamp_ns`
     volatile 不比）
- `--phase all` 跑全部；exit 0 = 全 PASS；exit 2 = ≥1 phase FAIL
- 沿 candidate 1 / 2 / 3 / 4 同款 spawn subprocess + workdir / role
  table / FakeStatePublisher 模板

### AC-Q: Quality Gate（不进 AC，但是 PASS 条件）

- `cargo fmt --all --check` clean
- `cargo clippy --workspace --all-targets -- -D warnings` clean
- `cargo clippy -p robot-cli --all-targets -- -D warnings` clean
- `cargo clippy -p robot-tests --bin robotctl_smoke --features
  robotctl-smoke -- -D warnings` clean
- `cargo build --workspace --release` 全过（含 robot-cli + robot-
  tests robotctl_smoke）
- `cargo build -p robot-cli --release` 编译过 + 二进制 <`size_limit`>
  （Plan 期定具体 KB 上限）
- `cargo test --workspace` PASS 计数严格大于 v0.4 候选 2 round 2
  完结期 989（候选 A 至少 +20 lib unit；Plan 期定具体目标）
- `cargo tree -p robot-cli --edges normal,no-proc-macro` 输出**不**含
  `nalgebra` / `urdf-rs` / 其它非候选 A 必须的 dep
- schema_lint：15 .fbs 全过（候选 A **0** 改 schema）
- `crash_restart_smoke --iterations 5`（real-s1 硬 gate）5/5 PASS
- `scripted_backend_smoke --phase all` 5/5 PASS（候选 1 regression）
- `ik_backend_smoke --phase all` 5/5 PASS（候选 2 round 2 regression）
- `pubsub_cleanup_smoke --phase all` PASS + `triggered_c3=false`
  （候选 3 regression）
- `clear_estop_e2e` PASS（候选 4 baseline；可选；Plan 期定）
- **`robotctl_smoke --phase all`**（本候选 A 硬 gate）全 phase PASS
- `cargo deny check`：**未新增 violation；overall fail 为既有
  baseline**（沿 round 1 + round 2 同款措辞；**严禁**写"过"）

---

## Reviewer round 1 拍板表 (Q-CLI1..Q-CLI15 全 ACK；2026-05-28 锁定)

reviewer 在 CodeMap 复审后已直接给出全部 Q1..Q15 拍板（详 reviewer
session log 2026-05-28）。本 Spec 沿 CodeMap §8 编号继续 Q-CLI1..
Q-CLI15；**不**重新开 Q16。

| Q | 主题 | 拍板 | 关联 AC |
|---|---|---|---|
| Q-CLI1 | 既有 iceoryx2 `estop` 子命令并存策略 | **(a)** 保留作 offline fallback；候选 A 不加 gateway `estop` stub（候选 D land 后再扩）| AC-2 (既有 3 子命令 0 改动) |
| Q-CLI2 | `robotctl` async runtime 模型 | **(b)** `main()` 入口分流；既有 status/watch/estop 保持 sync 路径；7 个新子命令走 tokio runtime block_on | AC-2 |
| Q-CLI3 | auth token 输入方式 | **(a)** `--token <TOKEN>` + env `ROBOT_TOKEN` fallback；缺 token exit 2 | AC-6 |
| Q-CLI4 | gateway 地址默认 | **(a)** 默认 `127.0.0.1:50051` + env `ROBOT_GATEWAY_ADDR` | §6.5 共享参数 |
| Q-CLI5 | `--idempotency-key` 默认行为 | **(b)** 自动生成 `cli-<request_id>`（复用本次 UUIDv7 request_id；**round 2 amend #2** 改此形以避免引 `rand`/`getrandom`，原 `cli-<unix_ms>-<random_hex_8>` 作废）；显式 retry 由用户传同 key；stderr 提示行 | AC-6 |
| Q-CLI6 | `tonic::Code` → exit code 映射详细度 | **(a)** 详细映射；Spec 必须列完整表 | AC-5 |
| Q-CLI7 | helper 抽到哪里 | **(b)** helper 抽到 `crates/robot-cli/src/lib.rs`；不污染 `crates/robot-rpc/`；不新增 helper crate | §影响范围 + AC-1 |
| Q-CLI8 | `--json` 输出范围 | 新 7 个 gateway 子命令支持 `--json`；既有 3 个 iceoryx2 子命令**不**改输出 | AC-7 + AC-2 |
| Q-CLI9 | smoke / 集成测试入口 | **(a)** 新 `robotctl_smoke` binary；feature-gated；真起 gateway + CLI 子进程 | AC-12 |
| Q-CLI10 | `stream-events` SIGINT 早退语义 | **(c)** exit 0 + stderr warning | AC-10 |
| Q-CLI11 | `invoke scripted` / `invoke ik` 默认 `--follow` | **(b)** 默认**不**follow；用户加 `--follow` 显式开启；不 follow 时 land action 后立即退出 | AC-9 + AC-7 |
| Q-CLI12 | `cancel-task` + `stream-events` 协作 | **(a)** 两个独立子命令组合（unix 工具哲学）；**不**加 `wait` 子命令 | AC-12 |
| Q-CLI13 | `--waypoints-file` 格式 | **(a)** JSON | AC-8 |
| Q-CLI14 | 顺手清理 smoke 4 处 `uuidv7_compat()` 重复 | **(b)** **不**清理；留独立 follow-up；robot-cli 自己用 `uuid::Uuid::now_v7()` 不复制 smoke inline | §严禁改动 + AC-6 |
| Q-CLI15 | Spec round 1 拍板范围 | **(a)** Q-CLI1..Q-CLI14 全部 + schema_id + exit code + 输出格式细表 | 全部 AC 落 |

**reviewer 额外锁定**（不开 Q-CLI16；直接进 AC + CLI contract 表）：

- `clear-estop --confirmation` 精确字面值 `CONFIRM_CLEAR_ESTOP`（全
  大写）+ `--audit-reason` 必填非空 + 缺失/不匹配 exit 2 → 详
  **AC-3** + §6.5 CLI contract 表

---

## reviewer 审查重点 (候选 A 实施期硬 gate)

1. **既有 3 个 iceoryx2 子命令行为 0 改动**（AC-2）：smoke phase 11
   `existing_subcommands_regression` 强 gate —— `status`/`watch`
   **deterministic byte-level diff = 0**（固定 published state）+
   `estop` exit code + `EStopEvent` body 字段等价（AC-2 精确判据；
   estop 自带时间戳不 byte-diff）；任何 reviewer round 中觉得有漂移
   直接 block
2. **`clear-estop` safety contract**（AC-3）：4 个失败路径（缺/匹配
   错/空白 confirmation × 缺/空 audit-reason）必须 **CLI 端早拒**
   `exit 2`，**不**发 RPC；smoke phase 3 `clear_estop_safety_
   contract_violations` 强 gate（实测 gateway audit log 无对应
   `clear_estop` invocation）
3. **`tonic::Code` → exit code 完整 exhaustive 映射**（AC-5）：lib
   `map_tonic_code_to_exit` exhaustive match；新 tonic Code variant
   编译 fail 强制更新；unit test 100% cover
4. **`--json` schema 稳定**（AC-7）：JSON 字段集 snake_case + ALL_CAPS
   enum + `<key>_ns` 整数 + 空字段不省略；smoke phase 10
   `json_output_round_trip` 强 gate
5. **`request_id` / `action_id` UUIDv7 本地校验**（AC-11）：CLI 端
   早拒 + 不污染 gateway audit log；lib unit 覆盖 3 个 fail path
6. **既有 4 个 smoke binary 0 改动**（§严禁改动）：候选 A 不顺手清
   理 4 处 `uuidv7_compat()` inline；保留独立 follow-up（Q-CLI14 (b)）
7. **候选 B/C/D RPC 0 land**（§严禁改动）：候选 A 仅 land 6 已 frozen
   RPC 的 client；`SetEnabled` / `GetState` / `StreamState` /
   `EStop` 触发 / Gripper 等任何缺口项**不**在候选 A scope
8. **Quality Gate 全过 + 4 个既有 smoke regression 0 退化**（AC-Q）
9. **cargo deny 措辞约定**：未新增 violation；overall fail 为既有
   baseline（沿 round 1 + round 2 同款；**严禁**写"过"）

---

## 不在范围（候选 A 边界）

候选 A 明确**不含**以下任一项：

- **既有 `status` / `watch` / `estop` 子命令任何改动**（行为 / 输出
  / exit code / 参数集；AC-2 锁定）
- **`crates/robot-rpc/` lib 任何改动**（候选 A 仅 client feature
  consumer；server / gateway / handler / dispatcher 0 触）
- **`crates/robot-msg/` schemas 改动**（9 fbs schema_id + `0x040B`
  + `0x040C` 不动）
- **`configs/rpc_roles.toml` 改动**（4 role × 9 capability 矩阵 +
  4 fake token 不动）
- **`docs/00_design.md` 正文改动**（§5.2 / §5.3 14 frozen RPC /
  §5.4 / §6.7 不动）
- **`deny.toml` baseline 修复**（沿 round 1 / round 2 同款 follow-
  up 节奏；独立 PR）
- **`SetEnabled` / `GetState` / `StreamState` / `EStop` 触发 / Gripper
  RPC 任一 land**（候选 B/C/D scope）
- **Python gRPC stub 生成 / PyPI 包 / dimos-deploy wrapper**（候选
  4 light Research defer；候选 E 重启后 outline）
- **OIDC / mTLS / keychain token store**（v0.5+ 真 auth 单独 outline）
- **`robotctl_smoke` 之外的其它 smoke binary 改动**（既有 4 个不动）
- **`robotctl` 子命令子层化**（如 `robotctl ai chat` / `robotctl
  config` 等运维工具扩展；不在候选 A 范围）
- **mDNS / service discovery / multi-robot 切换**（沿 Q-CLI4 (a)
  本地 loopback 优先；多 robot 切换走未来候选）
- **`--config <PATH>` 文件加载客户端 default**（候选 A 仅 env +
  command-line flag；client-side config file 留 v0.5+ 独立 outline）
- **`stream-events` follow 模式 + `wait` 子命令**（Q-CLI11 (b) +
  Q-CLI12 (a) 锁定不加）

---

## 跟其它 milestone handoff / spec 的链接

- [`team-swe/codemap/v0_5_operator_cli.md`](../codemap/v0_5_operator_cli.md)
  Pre-Research / CodeMap（commit `4714257`；含 §0 修订 + §1-§7 摸底
  事实 + §8 Q1..Q15 拍板候选选项；本 Spec 沿用全部）
- [`docs/handoffs/v0.5_entry.md`](../../docs/handoffs/v0.5_entry.md)
  §2.1 候选 A 范围（commit `0491f58` 已修订"首次 land → 扩展既有"）
  + §3.2 排第一位的理由 + §4.1 范围 + §4.3 RIPER 流程 + §5 hard
  boundaries 8 条 + §6 链接段
- [`docs/handoffs/v0.4_dimos_research.md`](../../docs/handoffs/v0.4_dimos_research.md)
  §6 7 个候选 `robotctl` CLI 工具表（候选 A 7 子命令的源头）
- [`docs/handoffs/v0.4_robot_kinematics_round2_summary.md`](../../docs/handoffs/v0.4_robot_kinematics_round2_summary.md)
  关键契约审计（IkBackend 8 步 semantic check + 3 类
  `ActionStartError` 严格分离 + source_task_id 单调）— 候选 A
  `invoke ik` 子命令 wire format 直接复用
- [`docs/handoffs/v0.4_scripted_backend_summary.md`](../../docs/handoffs/v0.4_scripted_backend_summary.md)
  `ScriptedPayload` fbs builder + gateway 两道 byte gate — 候选 A
  `invoke scripted` 子命令 wire format 直接复用
- [`docs/handoffs/v0.3_b4_summary.md`](../../docs/handoffs/v0.3_b4_summary.md)
  §3 7 条契约 + §4 cross-process e2e harness 工程经验 — 候选 A
  `robotctl_smoke` 沿用全部 7 契约
- [`docs/handoffs/v0.3_a1_4_clear_estop_smoke.md`](../../docs/handoffs/v0.3_a1_4_clear_estop_smoke.md)
  ClearEStop control plane 真闭环 — 候选 A `clear-estop` 子命令复
  用 6-12 step server side；client 端就 build + send 一次 + safety
  contract（AC-3）
- `crates/robot-rpc/proto/robot_control.proto` — 6 RPC + CommandMeta
  + AuthContext + 5 个 enum 字面权威（CodeMap §3 提取）
- `configs/rpc_roles.toml` — 4 role × 9 capability + 4 fake token
  矩阵权威（CodeMap §5）
- `crates/robot-tests/src/bin/{scripted,ik,pubsub_cleanup,clear_
  estop}_*_smoke.rs` — 4 份 tonic client helper 模式参考（CodeMap
  §4；候选 A inline 复制部分模式但**不**直接复用文件 + Q-CLI14 (b)
  锁定不清理 4 处 uuidv7_compat 重复）
- `crates/robot-cli/src/bin/robotctl.rs` — 既有 3 子命令 + signal
  handler pattern + CLI parser scaffolding（候选 A 扩 7 arm 不重搭）
- `docs/00_design.md` §5.2 / §5.3 / §5.4 / §6.7（候选 A **0** 触）

---

## 后续：reviewer round 2 + Plan

1. **reviewer round 2**（如有）：本 Spec round 1 拍板表 Q-CLI1..Q-CLI15
   reviewer 已 ACK；Spec 本身 reviewer round 2 决定（i）是否额外加
   AC（如 lib unit 详细数 / 测试覆盖率门槛 / `robotctl --version` 显示
   规则）；(ii) 是否扩 `--config <PATH>` 客户端配置文件 default
   （v0.5+ 单独 outline，不进候选 A）
2. **Plan outline**：`team-swe/specs/v0_5_operator_cli.md` 同文件
   §Plan 段或新 `team-swe/plans/v0_5_operator_cli.md`（reviewer Plan
   round 1 决定）；沿 candidate 1 / 2 / 3 / 4 / round 1 / round 2
   同款 §P.0..P.4 模板
3. **Plan round 拍板**：reviewer round 1 + 必要时 round 2-N（沿
   candidate 1-4 多轮 amend 节奏）
4. **commit 拆分**：粗估 ~5-8 commit（沿 v0.5_entry §2.1）；Plan 期
   定具体段顺序 + commit 拆分边界
5. **summary**：`docs/handoffs/v0.5_operator_cli_summary.md`（沿
   candidate 1-4 / round 1 / round 2 同款 milestone anchor 模板）

---

## Plan

> Plan outline draft v1（reviewer Plan round 1 ACK pending）。基于
> Spec §AC + §Reviewer 审查重点 + Q-CLI1..Q-CLI15 拍板 + round 2 amend
> 7 项**硬输入**；**不**重新决策任何 AC 边界 / Q-CLI 拍板 / hard
> boundary / §6.7 不变量。Spec round 2 amend commit `12cc834`
> (reviewer ACK) 之上开 Plan round 1。

### P.0 工作基线

- 起点：`master` HEAD = `12cc834`（Spec round 1 + round 2 amend land；
  reviewer round 2 ACK）。
- `crates/robot-cli/` 23 + 15 + 426 = 464 LoC 既有（Cargo.toml + 空
  lib.rs + 3 iceoryx2 子命令 `status` / `watch` / `estop`）；候选 A
  扩 ~600-900 LoC（lib client harness + 7 子命令 + 入口分流 +
  robotctl_smoke）。
- 不开新分支（沿候选 1-4 / round 1 / round 2 同款直接 commit 到
  master + 每段 reviewer ACK 后 push 再开下一 commit）。
- Q 拍板硬输入（详 §Reviewer round 1 拍板表）：Q-CLI1(a) estop offline
  fallback 保留 / Q-CLI2(b) main 入口分流 / Q-CLI3(a) `--token` >
  `ROBOT_TOKEN` > exit 2 / Q-CLI4(a) `127.0.0.1:50051` + env / Q-CLI5(b)
  `cli-<request_id>` / Q-CLI6(a) 详细 exit code 表 / Q-CLI7(b) helper
  入 robot-cli lib / Q-CLI8 新 7 子命令 `--json` / Q-CLI9(a) 新
  robotctl_smoke / Q-CLI10(c) SIGINT exit 0 + warning / Q-CLI11(b)
  默认不 follow / Q-CLI12(a) cancel + stream 独立 / Q-CLI13(a) waypoints
  JSON / Q-CLI14(b) 不清理 smoke uuidv7_compat / Q-CLI15(a) Spec 拍透。
- 全程 docs-only baseline 不动（`deny.toml` / `00_design.md` / frozen
  schema / rpc_roles.toml；详 §严禁改动）。

### P.1 commit 节奏总览

**Happy path**（AC-1..AC-12 全过 + AC-Q Quality Gate 全过）：5 commit +
1 summary。按 **RPC 家族**分组（同族 helper / 校验 / 输出复用度高，且
每段独立可回归）：

```
✨ feat: v0.5 候选 A (1/5) robotctl 扩 tonic 脚手架 + 共享 client lib
        harness land（Cargo.toml 扩 7 dep + main 入口 sync/async 分流
        Q-CLI2(b) + lib mod gateway_client/meta/exit_code/validate/
        payload/json_out/uuid_v7 + 纯路由函数 subcommand_lane(by-name)
        + 共享 lib unit：exit_code exhaustive 映射 / token 解析三态 /
        source env / idempotency cli-<request_id> / uuid_v7 / action-id
        UUIDv7 校验 AC-11 / 路由全表；**手写 parser 不动**、**不落可达
        新子命令 stub**，既有 3 子命令回归锁）
✨ feat: v0.5 候选 A (2/5) clear-estop safety contract + clear-error
        land（AC-3 四道闸 CLI 早拒 6 fail-path lib unit **先于 e2e** +
        clear-error 写 RPC + Ack 输出 human/--json）
✨ feat: v0.5 候选 A (3/5) get-task-status + cancel-task + stream-events
        land（TaskHandleRef 族 3 子命令 + action-id 复用 AC-11 校验 +
        TaskStatus 三态输出 AC-7 + stream-events SIGINT exit 0 + warning
        AC-10 + NDJSON 帧输出）
✨ feat: v0.5 候选 A (4/5) invoke scripted + invoke ik land（waypoints
        JSON parse + ragged 检查 AC-8 + IkPayload/ScriptedPayload fbs
        build inline 入 robot-cli lib Q-CLI7(b) + --follow Q-CLI11(b)
        默认关 + InvokeTask 路径）
✨ feat: v0.5 候选 A (5/5) robotctl_smoke cross-process e2e land（真
        spawn robotctl 子进程 + 11 phase 含 clear_estop_auth_denied_
        after_local_gate + safety_contract_violations + stream_events_
        sigint(SIGINT) + existing_subcommands_regression；feature gate）
📝 docs: v0.5 候选 A operator CLI summary 归档 + 候选 A 完结
```

粗估 5 commit + summary = 6 commit chain（Spec §交付 ~5-8 区间内）；如
reviewer round 出现拆分诉求，沿候选 1/3 同款拆为 (X/5-a)+(X/5-b)（走
§P.4 Plan-Execute diff 登记）。

**实际 commit chain（实施期追加，reviewer (1/5) 复审要求登记）**：每个
RPC 家族 commit 旁如带**计划外 docs commit**（依赖白名单 / diff 归档
等），都在此累计，避免最终"5 commit + summary"计数失真。已落：

| 段 | feat commit | 附带 docs commit | 备注 |
|---|---|---|---|
| (1/5) | `809575f` | `44375a4`（计划外：tonic client 依赖白名单登记，§P.4 #A1）| 2 commit |
| (1/5) 登记 | — | `3c664fc`（§P.4 (1/5) diff #A1-A4 归档 + 本表）| 1 commit |
| (2/5) | `2abde1d`（amend 自 055ca36）| `d107762`（非阻断：robot-rpc task_rpc_e2e stale 注释）+ `2dc5b32`（§P.4 #B1-B5 + 本表）| feat 1 + docs 2 |
| (3/5) | `239f2e0` | 本登记 docs（§P.4 #C1-C3 + §What line 70/73 viewer+ 措辞 + 本表）| feat 1 + docs 1 |
| (4/5) | `d2ceb12` | `7bab8b7`（🐛 fix：拒 wire 不支持的 --dry-run，#D6）+ 本登记 docs（§P.4 #D1-D6 + 本表）| feat 1 + fix 1 + docs 1 |
| 接班 | — | `202185a`（(5/5) entry handoff spec + reviewer 3 约束）| docs 1 |
| (5/5) | `25c2feb` | 本 summary docs（§P.4 #E1-E5 + summary 归档 + CLAUDE.md 状态 + README）| feat 1 + docs 1 |
| **候选 A 完结** | — | 5 feat + 1 fix + 7 docs = **13 commit**（`809575f` → 本 docs）| — |

#### P.1.1 reviewer Plan round 1 审查 5 重点对账

reviewer 进 Plan 时点名的 5 项，逐条映射落点（**实施期硬 gate**）：

| # | reviewer 审查重点 | Plan 落点 | 怎么满足 |
|---|---|---|---|
| 1 | commit 拆分让既有 3 子命令**始终**可回归 | §P.2.1 (async dispatch 脚手架 + 纯路由函数 + lib unit) + 每段 §Quality Gate + §P.2.5 phase 11 | (1/5) **手写 parser 不动**（仍只识别 status/watch/estop；新名 → unknown subcommand exit 2，robotctl.rs:110→:210），唯一新增是 main 顶 async dispatch 脚手架 + 纯路由函数 `subcommand_lane(name)`（lib unit 覆盖 10 名全表：3 sync + 7 async）+ 既有 robotctl unit 全保；(2/5)..(4/5) 每子命令**同 commit** append { parser 识别 + dispatch arm + 实现 + lib unit }，不碰既有 sync 路径；(5/5) `existing_subcommands_regression` phase 真起 s0 跑 3 子命令（判据 AC-2 精确两类）；**每个 commit 的 Quality Gate 都重跑既有子命令冒烟** |
| 2 | clear-estop 四道闸**先 lib unit 再 e2e** | §P.2.2 (lib unit, commit 2) **严格早于** §P.2.5 (e2e, commit 5) | 四道闸（confirmation 字面值 / audit-reason 非空 / token 可解析 / 缺任一）的 6 fail-path lib unit 在 (2/5) land 并过；e2e `clear_estop_safety_contract_violations` phase 在 (5/5) 才 land；commit 顺序保证 lib unit 先行 3 个 commit |
| 3 | robotctl_smoke **真测 CLI 子进程**不绕过 | §P.2.5 hard contract | smoke 只通过 `tokio::process::Command::new(bin_dir.join("robotctl"))` spawn CLI 子进程 + 读 stdout/stderr/exit code；**严禁** smoke 内 `import` robot-rpc client 直接调 tonic（CLI 才是 unit-under-test）；该约束写进 (5/5) 模块 doc + reviewer round gate |
| 4 | stream-events SIGINT **稳定复现** | §P.2.3 (handler) + §P.2.5 phase 7 (readiness-gated repro) | handler 用 `tokio::select!` { stream.next() , ctrl_c() }；smoke 复现去 race：先 invoke 一个**长** action（scripted 多段 total ≥ 数秒）→ stream-events 启动 → **读到首行 `event=` 才确认 stream 已开**（readiness gate）→ 再发 SIGINT → 有界等待 exit 0 + stderr warning；不靠固定 sleep 赌时序 |
| 5 | cargo deny 措辞 | 每段 §Quality Gate + §P.2.5.d | 一律写 **"未新增 violation；overall fail 为既有 baseline"**（assert_no_alloc BSD-1-Clause + serialport MPL-2.0 + unescaper GPL-3.0/MIT 三条既有）；**严禁**写"过 / 通过" |

### P.2 工作流（按 commit 顺序）

#### P.2.1 [commit (1/5)] 脚手架 + 共享 client lib harness

##### P.2.1.a `crates/robot-cli/Cargo.toml` 扩 7 dep + 入口分流

| # | 改动 | 说明 |
|---|---|---|
| 1 | `[dependencies]` append 7 条 | `robot-rpc = { workspace = true, features = ["client"] }` + `tonic` + `tokio`(features macros/rt-multi-thread/signal/io-util) + `flatbuffers` + `uuid` + `serde`(derive) + `serde_json`（全 `{ workspace = true }`；既有 dep 0 动；**不含 clap**，P1-2；AC-1）|
| 2 | `src/bin/robotctl.rs` `fn main()` 顶部 async dispatch **脚手架** | parse 后：`Command::{Status,Watch,Estop}` 走原 sync `run()`（不变）；其余走 `tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(async_dispatch(&args))`（Q-CLI2(b)；AC-2）。**(1/5) async 分支为空骨架**（无任何新 `Command` variant；新 variant 随各自 impl commit 加，P1-3）|
| 3 | 纯路由函数 `subcommand_lane(name: &str) -> Lane`（**手写**，不引 clap，P1-2）| **(1/5) 手写 parser（robotctl.rs:96 `Args::parse`）不扩新 variant**；`subcommand_lane` 是纯数据表函数（10 名 → Sync/Async lane），(1/5) 即可单测全表；新子命令的 parser 识别 + arg 解析 + 共享 flag（`--gateway-addr`/`--token`/`--source`/`--idempotency-key`/`--timeout-ms`/`--dry-run`/`--json`）+ help/usage 随**各自 impl commit**(2/5..4/5) append，**不在 (1/5)**（P1-3：避免可达 stub）|

##### P.2.1.b `crates/robot-cli/src/lib.rs` 共享 harness module

从空 lib 扩（模块切分为 §P.3 拍板 D2，可微调）：

| module | 职责 | 关键 API |
|---|---|---|
| `gateway_client` | 连接 + readiness | `connect_gateway(addr, connect_timeout, timeout) -> Result<RobotControlClient<Channel>, ExitCode>`（默认 `127.0.0.1:50051` + env `ROBOT_GATEWAY_ADDR`，Q-CLI4(a)；connect_timeout=2s 沿候选 1-4）。**⚠ (2/5) 已偏离此签名 → §P.4 #B3**：实际为 `connect_gateway(addr, connect_timeout) -> Result<_, String>`（去 per-request `timeout` 参），单 RPC deadline 改由 `dispatch::complete_ack_rpc` 的 `tokio::time::timeout` 独占（稳定 exit 6）|
| `meta` | CommandMeta 构造 + token | `build_command_meta(args) -> CommandMeta`（round 2 amend #2：request_id 一次生成复用、`cli-<request_id>`、`default_cli_source()` 走 USER/USERNAME）+ `resolve_token(args) -> Result<String, ExitCode>`（`--token` > `ROBOT_TOKEN` > exit 2）|
| `exit_code` | tonic Code → exit | `map_tonic_code_to_exit(code: tonic::Code) -> ExitCode` **exhaustive 不留 `_`**（AC-5 全表；新 Code variant 编译 fail 强制更新）|
| `validate` | 早拒校验 | `validate_action_id(&str) -> Result<(), ExitCode>`（UUIDv7：空/非 UUID/非 v7，AC-11；复用 `robot_rpc::auth::validate_request_id`）+ `check_clear_estop_gates(args) -> Result<(), ExitCode>`（AC-3 四道闸）|
| `payload` | fbs body 构造 | `build_scripted_payload(&[Waypoint]) -> Vec<u8>` + `build_ik_payload(target, quat, seed, frame) -> Vec<u8>`（inline 自 smoke 模式复制，Q-CLI7(b)；**不**入 `robot-msg/build_helpers`）|
| `json_out` | 输出 schema 化 | 各子命令 `#[derive(Serialize)]` 输出 struct + `print_human()` / `print_json()`（snake_case + `<key>_ns` 整数 + ALL_CAPS enum + 空字段不省略，AC-7）|
| `uuid_v7` | request_id 生成 | `new_request_id() -> String = Uuid::now_v7().hyphenated().to_string()`（Q-CLI14(b)；不复制 smoke `uuidv7_compat`）|

##### P.2.1.c 共享 lib unit（本 commit land）

- `exit_code`：每个 tonic::Code → 期望 exit code（AC-5 全表 exhaustive 覆盖）
- `meta::resolve_token`：3 态（`--token` 优先 / 回退 `ROBOT_TOKEN` / 都缺 → exit 2）
- `meta::default_cli_source`：USER 命中 / USERNAME 回退 / 都缺 `"unknown"`
- `meta::build_command_meta`：idempotency 自动 = `cli-<request_id>` / 显式 `--idempotency-key` 覆盖 / request_id 与 idempotency 同源
- `uuid_v7::new_request_id`：格式 + version=7 + variant 校验
- `validate::validate_action_id`：空 / 非 UUID / 非 v7 / happy（AC-11 4 path）
- 路由函数 `subcommand_lane(name)`：10 名全表（`status`/`watch`/`estop`→Sync、7 新名→Async）纯函数单测（reviewer 审查重点 1；parser 不需识别新名即可测路由决策）

##### P.2.1.d Quality Gate（commit (1/5)）

- `cargo fmt --all --check` clean
- `cargo clippy -p robot-cli --all-targets -- -D warnings` clean（exhaustive match 不留 `_` 兜底）
- `cargo clippy --workspace --all-targets -- -D warnings` clean
- `cargo build -p robot-cli --release` 过 + `cargo build --workspace --release` 过
- `cargo tree -p robot-cli --edges normal,no-proc-macro` **不**含 `nalgebra`/`urdf-rs` 等非必须 dep（AC-Q）
- `cargo test -p robot-cli` 全过（本 commit 新 lib unit + 既有保）
- 既有 3 子命令冒烟（机制 §P.3 拍板 D1）：`status`/`watch`/`estop` exit code + 输出结构不变
- `cargo deny check`：未新增 violation；overall fail 为既有 baseline

##### P.2.1.e 风险锚点

- **入口分流是唯一碰既有路径的改动**：sync 路径绝不进 tokio runtime；既有 SIGSEGV-mitigation signal handler（`libc::_exit(0)`）不动；async 路径独立用 `tokio::signal::ctrl_c()`
- **`uuid` v7 feature 传递引入 `getrandom`**：是既有 workspace dep 子树（robot-rpc 已用），非候选 A 新增直接依赖；`cargo tree` 验无意外 transitive
- **不落可达 stub（P1-3）**：(1/5) **不**加任何新 `Command` variant、**不**写 `unimplemented!()`；parser 不识别新名 → `robotctl clear-estop` 等 = "unknown subcommand" **exit 2**（既有行为 robotctl.rs:110→:210-213，不 panic / 不违 AC-5 exit code 契约）。(1/5) 只锁脚手架 + 共享 lib + 纯路由函数，不发任何 RPC（也不需 gateway）；新子命令首次"可达"即其完整实现 land 之时（2/5..4/5）

#### P.2.2 [commit (2/5)] clear-estop safety contract + clear-error

> **reviewer 审查重点 2 核心**：四道闸 lib unit 在本 commit land 且过，
> **早于** (5/5) e2e 3 个 commit。

##### P.2.2.a clear-estop 四道闸（AC-3）

- `validate::check_clear_estop_gates(args)` 顺序短路：
  1. token 可解析（`--token` > `ROBOT_TOKEN`；缺 → exit 2）
  2. `--confirmation` **精确** `CONFIRM_CLEAR_ESTOP`（全大写 ASCII；任何变体/前后空白拒 → exit 2）
  3. `--audit-reason` 必填 + 非空 + 非纯空白（拒 → exit 2）
- 任一闸失败：stderr 打 AC-3 精确文案 + exit 2 + **不发 RPC**（不污染 gateway audit log）
- 全过 → `build_command_meta` + `ClearEStop` RPC；server 端仍校验 confirmation（CLI first defense / server last defense）；server 拒透传 exit per AC-5

##### P.2.2.b clear-error（AC-4）

- 必填：`--audit-reason` 非空 + token 可解析；`ClearError` RPC；Ack 输出（`cleared_at_ns` + `dry_run`）human + `--json`

##### P.2.2.c lib unit（本 commit land；reviewer 审查重点 2）

- clear-estop 6 fail-path（缺/匹配错/空白 confirmation × 缺/空 audit-reason 笛卡尔去重，AC-3）
- clear-estop happy gate（4 参数齐 → gate 过，**不**真发 RPC，纯校验函数返 Ok）
- clear-error：缺 audit-reason / 空 audit-reason / 缺 token（3 path）
- Ack 输出 round-trip：固定 Ack → human + `--json` 精确 byte（AC-7）

##### P.2.2.d Quality Gate

- 同 P.2.1.d 全套 + 本 commit 新 lib unit 过 + 既有子命令冒烟不变 + cargo deny 措辞照旧

#### P.2.3 [commit (3/5)] get-task-status + cancel-task + stream-events

> TaskHandleRef 家族（`<action-id>` 位置参数 + AC-11 校验复用）。

##### P.2.3.a 三子命令（AC-4 / AC-7 / AC-10 / AC-11）

| 子命令 | RPC | 关键 |
|---|---|---|
| `get-task-status <id>` | `GetTaskStatus` | action-id 校验 → 单次 RPC → TaskStatus 三态输出（EXECUTING/COMPLETED{outcome}/FAILED{failure}，AC-7）；NotFound → exit 4 |
| `cancel-task <id>` | `CancelTask` | action-id 校验 + `--audit-reason` 非空 → Ack 输出 |
| `stream-events <id>` | `StreamTaskEvents` | action-id 校验 → server-streaming 每帧一行 stdout（human / NDJSON）；终态(COMPLETED/FAILED/CANCELLED)自动关 exit 0；SIGINT → exit 0 + stderr warning（AC-10）；`--timeout-ms` 整流总时长到期 → exit 6 |

##### P.2.3.b stream-events SIGINT handler（reviewer 审查重点 4）

```rust
loop {
    tokio::select! {
        item = stream.message() => match item { ... 终态 break exit 0 / 帧 print / err exit per AC-5 },
        _ = tokio::signal::ctrl_c() => {
            eprintln!("warning: stream interrupted before terminal event");
            return ExitCode::from(0);
        }
    }
}
```

- 只处理 SIGINT（`ctrl_c()`）；SIGTERM 不接管（round 2 amend #4 已同步 smoke 发 SIGINT）

##### P.2.3.c lib unit

- TaskStatus 三态 → human + `--json`（固定 proto 入 → 精确 byte，AC-7）
- 帧事件 STARTED/PROGRESS/COMPLETED/FAILED/CANCELLED → 一行格式（human + NDJSON）
- action-id 校验复用 P.2.1.c（不重复，引用）
- timeout 计算（`--timeout-ms` → Deadline）

> SIGINT 路径本身（信号触发）不在 lib unit 覆盖（信号难单测）；放 (5/5) smoke
> phase 7 readiness-gated 复现（reviewer 审查重点 4）。

##### P.2.3.d Quality Gate — 同上全套

#### P.2.4 [commit (4/5)] invoke scripted + invoke ik

##### P.2.4.a invoke scripted（AC-8）

- `--waypoints-file <PATH>` 必填存在 → `serde_json` parse → struct `{ waypoints: [{ positions: [f64], dwell_ms: u32, segment_duration_ms: u32 }] }`
- **round 2 amend #3**：positions 长度由 JSON 决定；CLI 仅 ragged 等长检查（不等 → exit 2）；关节数交 server `ScriptedBackend`（mismatch → exit 4）
- `payload::build_scripted_payload` 构 fbs → `InvokeTask` kind=Scripted；Ok → `action_id`；`--follow`(Q-CLI11(b) 默认关) → 接 stream-events 到终态

##### P.2.4.b invoke ik（AC-9）

- 必填 `--target-x/y/z`(f64) + `--audit-reason` + token；可选 `--target-qx/qy/qz/qw`(默认 `0 0 0 1`) + `--seed-joints` + `--frame` + `--follow`
- quaternion **CLI 不校验**（透传 4 f64，server IkBackend 校验）
- `payload::build_ik_payload` 构 fbs（SCHEMA_IK_PAYLOAD 0x040C）→ `InvokeTask` kind=Ik

##### P.2.4.c lib unit

- waypoints JSON：happy + 5 fail（文件缺/JSON 语法/缺 waypoints/ragged/类型错，AC-8）
- scripted payload fbs build → round_trip verify（root + 字段读回）
- ik payload fbs build：happy + invalid f64（AC-9）+ default quat（`0 0 0 1`）+ round_trip verify

##### P.2.4.d Quality Gate — 同上全套

#### P.2.5 [commit (5/5)] robotctl_smoke cross-process e2e

##### P.2.5.a hard contract（reviewer 审查重点 3）

- 新 `crates/robot-tests/src/bin/robotctl_smoke.rs` + feature `robotctl-smoke`（feature-gated；`[[bin]]` required-features）
- inline 复制 `scripted_backend_smoke.rs`/`ik_backend_smoke.rs` 同款 CaseCtx / FakeStatePublisher / SubprocessHandle / bring_up / tear_down（**不**抽公共库；Q-CLI14(b) 不清理既有 uuidv7_compat）
- **唯一被测路径 = spawn `robotctl` 子进程**：`tokio::process::Command::new(args.bin_dir.join("robotctl"))`；**严禁** smoke 内直接调 tonic client（CLI 才是 unit-under-test）；模块 doc 顶部明文写此约束

##### P.2.5.b phase 集合（首版 11；最终 §P.3 拍板 D3）

1. `selftest` — bring-up gateway + s1_daemon（+ s0 for regression phase）；不调 CLI；验 harness
2. `clear_estop_auth_denied_after_local_gate`（round 2 amend #5）— viewer token + 合法 4 参数 → CLI 四道闸过 → RPC 发出 → server PermissionDenied → exit 3
3. `clear_estop_safety_contract_violations` — 4 fail case 全 exit 2 + **gateway audit log 无对应 clear_estop invocation**（验 CLI 早拒不发 RPC，reviewer 审查重点 2 e2e 侧）
4. `get_task_status_happy` — invoke ik 拿 action_id → get-task-status → exit 0 + state ∈ {EXECUTING, COMPLETED}
5. `cancel_task_happy` — invoke scripted 长 action → cancel-task → exit 0 → get-task-status = CANCELLED
6. `stream_events_terminal_completed` — invoke ik → stream-events → 终态 COMPLETED → exit 0 + 末行 `event=COMPLETED`
7. `stream_events_sigint`（reviewer 审查重点 4）— invoke 长 scripted → stream-events → **读到首 `event=` 行（readiness gate）** → 发 **SIGINT** → 有界等待 exit 0 + stderr warning
8. `invoke_scripted_happy` — `--waypoints-file <tmpfile>` happy JSON → exit 0 + `action_id`
9. `invoke_ik_happy` — `invoke ik --target-x 0.20 --target-y 0.0 --target-z 0.10` → exit 0 + `action_id`
10. `json_output_round_trip` — 各 happy phase 加 `--json` → stdout `serde_json::from_str` 过 + 字段集 cover AC-7
11. `existing_subcommands_regression`（reviewer 审查重点 1）— 真起 s0_daemon → 跑 `status`/`watch`/`estop` → exit code + 输出判据（§P.3 拍板 D1）

- `--phase all` 跑全部；exit 0 = 全 PASS / exit 2 = ≥1 FAIL

##### P.2.5.c Quality Gate（候选 A 完结硬 gate；AC-Q 全集）

- fmt / clippy(workspace + robot-cli + robotctl_smoke feature) / build(workspace release) 全 clean
- `cargo test --workspace` PASS 计数 > 989（候选 A ≥ +30 lib unit；最终数 §P.3 拍板 D4）
- `robotctl_smoke --phase all` 全 PASS（**本候选硬 gate**）
- 4 既有 smoke regression 0 退化：`crash_restart_smoke` 5/5（real-s1）+ `scripted_backend_smoke` 5/5 + `ik_backend_smoke` 5/5 + `pubsub_cleanup_smoke` all PASS + `triggered_c3=false`
- schema_lint 15 .fbs 全过（候选 A 0 改 schema）
- `cargo tree -p robot-cli` 无意外 dep
- `cargo deny check`：**未新增 violation；overall fail 为既有 baseline**（严禁写"过"）

##### P.2.5.d 风险锚点

- **既有子命令需 s0**：regression phase 比前 4 个 smoke 多起一个 s0_daemon（mock adapter）；bring_up 扩 s0 spawn
- **regression 判据（AC-2 精确两类，P1-1）**：`status`/`watch` 在固定 published state 下输出真 byte 稳定（无自生 clock）→ **deterministic byte-level diff=0** 对 golden 字面；`estop` 自带 wall-clock `stamp_ns` 无 byte 稳定产物 → exit code + `EStopEvent` body 字段等价（不 byte-diff、不 mask）。golden 字面捕获机制走 §P.4 diff（实现细节）
- **SIGINT 复现**：靠 readiness gate（读首 `event=` 行）去 race，不靠固定 sleep

### P.3 reviewer Plan round 拍板（pending）

| Round | 状态 | 内容 |
|---|---|---|
| Plan round 1 | **复审完（不批准进入 (1/5)）** | 3 阻断（D1/AC-2 语义 → Spec round 3、clap、可达 stub）+ 1 非阻断（dep 数）；reviewer 对 D2-D5 方向无异议 |
| Plan round 1 amend | **本轮** | P1-1..P1-4 全吸收（详顶部 "Plan round 1 amend 记录"）；待 reviewer round 2 复审 |
| Plan round 2 | pending | amend 复审；ACK 后进 (1/5) |
| Plan round N | pending | 最终 Approved 进入 (1/5) 实施 |

**Plan 拍板项（D2..D5 待 reviewer Plan round 决定；D1 已由 Spec round 3
amend 在 AC-2 闭）**：

| # | 拍板项 | 候选 | 倾向 |
|---|---|---|---|
| D1（已闭）| `existing_subcommands_regression` 等价判据 | **已由 Spec round 3 amend（P1-1）在 AC-2 定死**：`status`/`watch` deterministic byte-level diff=0（FakeStatePublisher 固定 published state）+ `estop` exit code + `EStopEvent` body 字段等价；**非** mask 方案 | 不再 Plan 层拍板（语义归 AC-2）；golden 捕获 = 实现细节走 §P.4 |
| D2 | lib module 切分粒度 | (a) 7 module（gateway_client/meta/exit_code/validate/payload/json_out/uuid_v7）；(b) 合并为 3-4 个 | **(a)**（职责清晰；单测边界干净）|
| D3 | robotctl_smoke phase 最终集 | (a) 11 phase（§P.2.5.b）；(b) 增删 | **(a)**（已覆盖 7 子命令 + 4 reviewer 重点 + regression）|
| D4 | lib unit 数量目标 | (a) ≥30；(b) 更高门槛 | **(a)**（粗估 35-40；最终以实现为准）|
| D5 | commit 是否 5 段 | (a) 5 段（按 RPC 家族）；(b) 7 段（每子命令一段）；(c) 3 段（粗合并）| **(a)**（家族内 helper/校验/输出复用度高；段数适中且每段可回归）|

> `--config <PATH>` 客户端配置文件：Spec §不在范围已拍 **defer**（v0.5+
> 独立 outline）；**不**进 D 拍板项。

### P.4 Plan-Execute diff 登记位置

沿候选 1-4 / round 1 / round 2 §P.4 同款 appendix-only 模板。实施期偏离
Plan 任一决策 → 登记本节（**不**回退 Plan 正文），含：触发条件（哪个
Plan 决策被偏离）+ 实际选择 + 理由（reviewer ACK / 实施期发现）+ 验证
回归（4 既有 smoke + robotctl_smoke + AC-Q 全过）。

预期可能 diff：

- diff #1：smoke harness inline 复制行数实际 vs 预估
- diff #2：lib module 切分实际 vs D2（合并/再拆）
- diff #3：lib unit 数实际 vs D4（≥30）
- diff #4：`existing_subcommands_regression` 等价判据实现细节（D1 落地）
- diff #5：stream-events SIGINT 复现 readiness gate 实现（如固定 sleep 仍 flaky → 改 stdout 行读 gate）
- diff #6：commit 段数如 reviewer round 改 D5（5 → 拆/合）

#### 实际 diff（实施期登记；reviewer ACK 后归档，**不**回退 Plan 正文）

**(1/5)（commit `809575f` + `44375a4`；reviewer 2026-05-30 复审 ACK，diff
#A3 裁决"接受"）：**

| # | 触发（被偏离的 Plan 决策）| 实际选择 | 理由 / reviewer | 验证回归 |
|---|---|---|---|---|
| A1 | §交付未列依赖白名单 docs commit；2026-04-27 A1-1 "仅 robot-rpc link tonic/prost/tokio" 白名单 | (1/5) 追加 docs commit `44375a4` 把 robot-cli 登记进 tonic/tokio/prost consumer 白名单（docs/CHANGELOG.md 2026-05-29）| robot-cli 经 AC-1 reviewer ACK 成 client consumer，超越 A1-1 旧白名单，必须显式登记；reviewer 要求计入 commit chain（§P.1 实际表）| 1021 workspace test + s0/s1 release 不受影响（0 tonic 符号边界不变）|
| A2 | §P.2.1.b `payload` 列为 (1/5) 7 module 之一 | `payload` 推到 (4/5) invoke 子命令同 commit | payload 是 invoke-only（非 7 子命令共享），fbs build 宜跟 invoke 实现同 commit 验证；(1/5) 落 6 module，最终仍 7 module（D2 不变）| (1/5) 26 lib unit + clippy/build clean |
| A3 | §P.2.1.a item 2：async dispatch 脚手架落 `bin/robotctl.rs` main() | 脚手架（`block_on_async` + `subcommand_lane`）落 **lib**（已单测），bin main **0 改动** | main 内加 async 分支要么 dead code 要么让新名可达（违 P1-3）；落 lib 反而强化 P1-3（bin 字节未变 = 新命令 provably 不可达 exit 2）| 既有 3 子命令 + unknown 路径字节级不变；26 lib unit |
| A4 | **diff #3（reviewer 点名接受）**：workspace feature unification | robot-cli 无条件启用 `robot-rpc/client` → `cargo test --workspace` 默认跑 `task_rpc_e2e` 6 cross-process（之前需 `--features client`）；test 计数 1021 = 989 + 26(robot-cli) + 6(task_rpc_e2e) | reviewer 裁决"接受"：测试覆盖增加非生产路径放宽；**不**为保留"默认跳过"去 feature-gate robot-cli client dep（与候选 A "operator CLI 默认可用"冲突）；**要求写入最终 summary 的 Plan-Execute diff** | s0/s1 生产二进制单独构建不依赖 robot-rpc，0 影响 |

> **A4 → 最终 summary 必带**：候选 A summary 的 Plan-Execute diff 段必须
> 复述本条（task_rpc_e2e 默认运行）；A1（`44375a4`）计入 commit chain 计数。

**(2/5)（commit `2abde1d`，amend 自 055ca36；reviewer 2026-05-31 代码层 ACK，
条件：本 (2/5) diff 登记后方可 push）：**

| # | 触发（被偏离的 Plan 决策）| 实际选择 | 理由 / reviewer | 验证回归 |
|---|---|---|---|---|
| B1 | §P.2.1.b / D2 列 7 lib module | 实际 8 landed + payload 待 = 最终 **9**：新增 `cli_args`（gateway 子命令手写 parser）+ `dispatch`（async 分发 + handler + 编排 helper）| parser / 路由 / 编排分模块利于单测 + bin 极薄；同 #A3 逻辑（testability）| robot-cli 60 lib unit；clippy/fmt clean |
| B2 | §P.2.1.a item 3：新子命令 arg 解析 append 进 `bin/robotctl.rs` 手写 parser | gateway parser + dispatch + handler 全落 **lib**；bin 仅得 `main()` 入口分流 peek（既有 `Args`/`parse`/`run` 0 改）| 同 #A3：bin 字节最小化 + lib 最大可测；强化 AC-2"既有 3 子命令 0 改动"| 既有路由实测不变；`status` 仍走 sync banner exit 0 |
| **B3** | **§P.2.1.b `gateway_client` 行旧签名 `connect_gateway(addr, connect_timeout, timeout) -> Result<_, ExitCode>`**（本文件该行已加 ⚠ 内联标记）| 实际 `connect_gateway(addr, connect_timeout) -> Result<_, String>`：去 per-request `.timeout`，只留 connect timeout；单 RPC deadline 由 `dispatch::complete_ack_rpc` 的 `tokio::time::timeout` **独占** | reviewer 阻断 2：endpoint timeout 超时 code 不稳定（可能 `Cancelled`→exit 7）；CLI 层 tokio timeout 兜死 **exit 6**（AC-5 client timeout 语义）。`String` 错误由 call site 映射 exit 5(connect) | `complete_ack_rpc` 3 lib unit（含 `pending` future + 10 ms → 确定性 exit 6）；CLI connect-fail exit 5 实测 |
| B4 | （非 Plan 列）clear-error 共享 parser 收下 clear-estop 专属 `--confirmation` | `cli_args::reject_confirmation` 早拒（exit 2 `--confirmation is not valid for clear-error`），**不静默吞** | reviewer 阻断 1 | 4 lib unit + CLI 实测；clear-estop 仍接受 --confirmation |
| B5（in-commit 解决）| 前次 (2/5) 报告 diff #4「client-timeout→exit-6 推 (5/5)」| reviewer 拒推 → (2/5) 即用 `complete_ack_rpc` 锁 exit 6（见 B3）| reviewer 阻断 2：`--timeout-ms` 已在 (2/5) 暴露给 clear-estop/clear-error，exit 6 语义须同 commit 锁 | 见 B3 |

> **B3 审计提示**：读 §P.2.1.b `gateway_client` 行时**以本 #B3 为准**；Plan
> 正文旧签名（`timeout` 第三参 + `ExitCode` 返回）已废，不再是实现契约。

**(3/5)（commit `239f2e0`；reviewer 2026-05-31 代码层 ACK，条件：本 (3/5)
登记 + §What line 70/73 viewer+ 措辞修正后方可 push）：**

| # | 类别 | 内容 | 边界 / 依据 |
|---|---|---|---|
| **C1** | **⚠ 非安全保证澄清** | read RPC（`get-task-status` / `stream-events`）的 `--token` / `ROBOT_TOKEN` gate 是 **CLI 本地策略 gate**：缺 token → exit 2（AC-4 client-side），但 token **不上 wire**，CLI 不做 role / authz | `TaskHandleRef`（proto `robot_control.proto`）**无 auth 字段**；gateway `task_handlers::validate_task_handle_ref` 明确 v0.3 read RPC 服务端 **unauthenticated**（`GetTaskStatus` / `StreamTaskEvents` 当前任何 token 都能读）。**因此不能宣称 viewer+ 已被服务端强制**——"viewer+" 仅 (a) 未来 metadata auth 目标 或 (b) operator UX 语义。§What line 70/73 已同步改为"CLI requires token locally; server auth deferred to metadata path"措辞 |
| C2 | 口径澄清 | `stream-events --timeout-ms` = **开流 + 消费整流总时长**（`tokio::time::timeout` 包 stream_work；到期 `stream timed out after <MS>ms` → exit 6）；connect 独立走 `CONNECT_TIMEOUT` → exit 5 | AC-10"整流总时长（不是单帧）"落地口径 |
| C3 | 内部 refactor | `with_deadline<T,F>` 从 (2/5) `complete_ack_rpc` 泛型化（exit-6 语义不变）；get-task-status / cancel-task / clear-* 共用 | 无行为变化；沿 #B3 |

> **C1 不变量提示**：审计 / 后续 reviewer 看到 CLI 要求 token 时，**不得**据此
> 认为 read RPC 已 viewer+ 鉴权；服务端 read auth 留 v0.5+ metadata 路径
> （proto `TaskHandleRef` 加字段或走 tonic metadata）才落地。本段无模块数
> diff、无新依赖（D2 最终仍 9 = 8 landed + payload 待 (4/5)）。

**(4/5)（commit `d2ceb12` feat + `7bab8b7` fix；reviewer 2026-05-31 代码层
ACK，D3/D4/D6 ACK）：**

| # | 类别 | 内容 | 边界 / 依据 |
|---|---|---|---|
| D1 | 模块/parser 扩 | `GatewayArgs` +11 invoke 字段（共享 parser grows，§P.2.1.a item 3）+ `reject_invoke_flags_except(allowed)` 跨子命令白名单拒（非 invoke `allowed=[]`；scripted 放 `--waypoints-file`/`--follow`；ik 放 `--target-*`/`--seed-joints`/`--frame`/`--follow`）| 把阻断 1（不静默吞）推广到全 invoke-family flag |
| D2 | 模块落地 | `payload` 模块 land —— **解决 A2 推迟**（(1/5) payload 推到 (4/5)）| 最终模块数 **9**（与 B1 一致）；payload-only inline（Q-CLI7(b)），不抽 robot-msg |
| **D3**（ACK）| 安全本地校验 | `parse_waypoints_json` 额外拒空 waypoints（`waypoints must not be empty`），略超 AC-8"仅 ragged" | 避免 `[0]` index panic + 不发空 payload 的无意义 RPC（reviewer ACK：安全本地语义校验）|
| **D4**（ACK）| 边界澄清 | CLI **不**预 gate scripted payload 字节数（`MAX_SCRIPTED_PAYLOAD_BYTES`）；超限交 server → `FailedPrecondition` → exit 4 | 保持"CLI 构造透传，gateway/backend 负责容量与语义"边界（reviewer ACK）|
| D5 | 内部 refactor | `stream_to_terminal` 从 `run_stream_events` 抽出，stream-events + invoke `--follow` 复用 | 无行为变化 |
| **D6**（ACK）| **⚠ 非安全保证澄清 + 安全修复（同 C1 风格）** | `--dry-run` **仅 `clear-estop` 端到端支持**（`clear_estop.fbs` 带 dry_run + S0 action layer 真识别）；其余 6 子命令（clear-error / get-task-status / cancel-task / stream-events / invoke scripted / invoke ik）的 wire fbs **不带** dry_run（`grep dry_run schemas/` 仅 clear_estop），故 `reject_dry_run` 一律 exit 2 + 解释文案，**防"flag 接受但 wire 不携带 → 实发真实动作"的危险误导**。附：`run_invoke_scripted` 读文件改 `tokio::fs`（async 阻塞边界）；tokio + `fs` feature（Cargo.lock 未变）| commit `7bab8b7`（行为 + usage 字面同步，明文标注）。**(5/5) 跟进**：robotctl_smoke 加一组"6 非 clear-estop --dry-run → exit 2"安全矩阵 phase，防后续新子命令重开此洞（reviewer 建议）|

> **D6 不变量提示**：新增任何 gateway 写/读子命令时，**默认拒 `--dry-run`**，
> 除非其 wire fbs 真带 dry_run 字段 + daemon action layer 真识别（当前仅
> clear-estop）。robotctl_smoke 安全矩阵 phase 是该不变量的回归守卫。

**(5/5)（commit `25c2feb` feat + 本 summary docs；候选 A 完结）：**

| # | 类别 | 内容 | 边界 / 依据 |
|---|---|---|---|
| E1 | **判据适配（AC-2 status）** | §P.2.5.d 设想 status/watch 都 deterministic byte-diff=0；实测 `robotctl status` 单发 `latest()` 读受 iceoryx2 **late-subscriber 投递时机竞态**（投递在订阅者连接后下一次 publish；status 立即读 → 通常 "no sample yet"），非 robotctl bug。故 AC-2 byte-diff **主证走 `watch`**（persistent subscriber 循环 reliable 读 fixed sample → sample-block byte-identical + 含 layout）；status 只断言 exit 0 + 跨运行 byte-identical | status / watch 共用同一 `print_snapshot`（robotctl.rs:425）—— watch 证渲染 determinism 即覆盖 status 同路径 |
| E2 | 结构化审查重点 3 | robotctl_smoke **只** `dep:tokio + serde_json`，**不**引 robot-rpc/tonic → "smoke 不调 tonic client" 从纪律变编译期边界（物理上无法 import client）；serde_json 仅 json_output phase 真 parse | 比 Plan 设想更强（结构锁）〔**2026-08-27 Reverse Sync（候选 E 段(8) round 2，`v0_5_dimos_replace.md` PX-19）**：段(4) 起 robotctl_smoke 用 `robot_rpc::client::layout` 派生 layout，"不引 robot-rpc" 已破且单 feature 构建 E0433；边界改述为"**不启用 `robot-rpc/client`、不调 tonic client**"，feature 表 `robotctl-smoke += dep:robot-rpc`（不开 client、不拉 tonic），optional `robot-rpc` 撤全局 client、四 tonic client smoke 各自显式 `robot-rpc/client`〕|
| E3 | regression 不起 s0_daemon | §P.2.5.b 设想"真起 s0_daemon"；实际 status/watch/estop 全 iceoryx2-direct（不经 gateway/s1），用 **fixed-stamp `FakeStatePublisher`**（deterministic）+ `EStopCapture` 订阅验 body 即可，**无需 s0_daemon binary** | AC-2 fixed-state determinism 落地；preflight 只需 robotctl + gateway + s1 |
| E4 | dry_run 安全矩阵 phase | D6 的 (5/5) 跟进落地 = phase 12 `dry_run_safety_matrix`（6 非 clear-estop 子命令 + 不可达 gateway → 全 exit 2）| reviewer D6 建议的回归守卫 |
| E5 | 新 config | `configs/robotctl_smoke.toml`（5-DOF + backends = [mock_dwell, hold_pose, scripted, ik]）—— scripted happy + ik phase 都可 dispatch | 测试 fixture（非 frozen / 非 §6.7）|

> **候选 A 完结**：(1/5)–(5/5) + dry-run fix 全 land + pushed。AC-1..AC-12 +
> AC-Q 全过；§6.7 / design.md / B4 frozen schema / rpc_roles **0 触动**。详
> [`docs/handoffs/v0.5_operator_cli_summary.md`](../../docs/handoffs/v0.5_operator_cli_summary.md)。

---

## 后续：reviewer Plan round + 实施

Plan outline draft v1 commit 后开 Plan round 1；reviewer ACK（+ 必要时
round 2-N 阻断 fix）后进 (1/5) 实施。沿候选 1-4 / round 1 / round 2 同款
reviewer round 模式（outline → ACK → (1/5) → reviewer ACK push → (2/5)
→ ... → (5/5) → summary）。
