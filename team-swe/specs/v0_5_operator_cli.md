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

## What

为 v0.5 候选 A 落地 maintainer 本地 operator CLI 真实接入所需最小可
用闭环：

1. **扩展 `crates/robot-cli/`**（v0.2 Part 2 已 land；含 3 个 iceoryx2
   直连 S0 子命令 `status` / `watch` / `estop`）加 7 个 tonic gRPC
   子命令（**不**重新搭脚手架；既有 3 子命令行为 **0** 改动）：
   - `robotctl clear-estop`（maintainer+；`ClearEStop` RPC；safety-
     critical；§6.5 contract 严格锁）
   - `robotctl clear-error`（maintainer+；`ClearError` RPC）
   - `robotctl get-task-status <action-id>`（viewer+；`GetTaskStatus`
     RPC）
   - `robotctl cancel-task <action-id>`（operator+；`CancelTask` RPC）
   - `robotctl stream-events <action-id>`（viewer+；`StreamTaskEvents`
     RPC；server-streaming）
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
  * commit ~1：`crates/robot-cli/Cargo.toml` 扩 dep + 入口分流脚手
    架 + lib client harness module（fmt + clippy + 编译过；既有 3
    子命令回归过；7 新子命令 stub `unimplemented!()`）
  * commit ~2..7：7 个新子命令 land（顺序由 Plan 期定；建议沿 Q
    顺序：clear-estop → clear-error → get-task-status → cancel-task
    → stream-events → invoke scripted → invoke ik；每段独立 commit
    + 对应 lib unit）
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
- 既有 6 条 dep（`robot-core` / `robot-logging` / `robot-msg` /
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
- 既有 3 子命令 stdout / stderr / exit code **0** 改动；任一既有子
  命令在 `robotctl_smoke` regression phase 实测 round-trip 等价（输
  出 byte-level diff = 0）
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
  11. `existing_subcommands_regression`：跑既有 `status` / `watch`
     / `estop` 子命令 → 输出 + exit code byte-level diff vs baseline
     = 0
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
   `existing_subcommands_regression` byte-level diff = 0 强 gate；
   任何 reviewer round 中觉得有漂移直接 block
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

## Plan（占位；待 reviewer Plan round 1 进入）

### P.0 工作基线

- master HEAD = `ee4b47f`（Spec round 1 land；round 2 amend #7）+
  本 round 2 amend commit（docs-only，**代码基线不变**）；Plan round 1
  正式进入前再对齐到当时 master HEAD（round 2 amend 自身无法自引其
  commit hash，故此处锚 round 1 land 的 `ee4b47f`）
- `crates/robot-cli/` 23 + 15 + 426 = 464 LoC 既有；候选 A 扩 ~600-
  900 LoC（lib client harness + 7 子命令 + 入口分流）
- 工作目录：`crates/robot-cli/` + `crates/robot-tests/src/bin/robotctl
  _smoke.rs`（新）+ `crates/robot-tests/Cargo.toml`（扩 feature）+
  `configs/robotctl_smoke.toml`（如新建）

### P.1..P.4 待 Plan round 1 写

按 reviewer round 1 拍板顺序展开：

- §P.1 commit 节奏总览（拆段；最终 §P.3 拍板）
- §P.2 工作流（按拆段顺序展开）
- §P.3 Plan 拍板项（粗估 ~7-12 项；含 commit 拆分 / lib unit 数 /
  smoke phase 集合最终拍板 / `--config <PATH>` 是否进首版 / etc.）
- §P.4 Plan-Execute diff 登记位置（沿 candidate 1-4 / round 1 / round
  2 同款 appendix-only 模板）
