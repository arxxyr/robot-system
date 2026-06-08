# Spec · v0.5 候选 C — `SetEnabled` RPC（round 4）

> RIPER Spec round 4（r1 D2/D4/D5 ACK + r2 4 语义阻断 + r3 2 执行级阻断 + r4
> enable Ack point 缺口回修）。前置 =
> [`team-swe/codemap/v0_5_set_enabled.md`](../codemap/v0_5_set_enabled.md)。**不开实现。**

## Spec round 4 amend 记录（2026-06-08；reviewer Spec round 3 复核后）

reviewer：B5/B6 已修；新关键语义缺口 **B7 = `SetEnabled(true)` 成功 Ack point
未定义**（Ready accepted vs Enabled.Active actually enabled）。已回修：

| # | round 3 阻断 | 修订 |
|---|---|---|
| B7 | AC-2 `SetEnabled(true)→Ack` 没锁 Ack 在 Safe→Ready accepted 即返、还是 Ready+Heartbeat→Enabled.Active effectful `Enable(true)` ack 后返（`state_machine.rs:580/1124`）| **新 D8：Ack only after actual enable/disable completes**（reviewer 倾向，采纳；不选 accepted-only）。`SetEnabled(true)` 成功 Ack = **Enabled.Active commit + adapter `Enable(true)` ack**；Ready 但 deadline 内未到 Enabled.Active → `DeadlineExceeded`；adapter ack fail → `commit_fault(AdapterErr)` + `Internal` + domain `AdapterError`（与 disable 对称）。AC-2 改"观测 Enabled.Active 才算 success" + 加 AC-2b/2c；e2e 需 heartbeat 源 |

## Spec round 3 amend 记录（2026-06-08；reviewer Spec round 2 复核后）

reviewer：round 2 大方向已修；新暴露 2 执行级阻断（Plan/实施期直撞），已回修：

| # | round 2 阻断 | 修订 |
|---|---|---|
| B5（D3d）| `RpcStatus::AdapterError` **不存在**（control-plane `RpcStatus` 只有 Unknown/Ok/FailedPrecondition/PermissionDenied/InvalidArgument/Internal/DeadlineExceeded/ResourceExhausted/Aborted，`clear_estop.fbs:118`，A1 锁）| disable adapter 失败 wire `RpcStatus = **Internal**`（adapter ack fail = daemon 内部执行失败，非 precondition）；`SetEnabledResponse` 另带 **domain `SetEnabledStatus`**（候选 C 自有 fbs enum，如 `AdapterError`）供审计；gateway `Internal → tonic::Code::Internal`。**不扩 A1 共享 `RpcStatus`** |
| B6（D6）| auto_enable 按 resolved hardware_id，但 build_adapter 仍取 `.first()`（`s0_daemon.rs:1061`）→ CLI `--hardware-id right_arm` 时 auto_enable 读 right_arm 但 adapter 按 first（left_arm）→ D1/D6 不一致 | 加**单一 helper `resolve_hardware_section(cfg, hardware_id)`**：build_adapter + auto_enable **同段**；无匹配 → **fail-fast**。§影响范围 + AC-8 纳入 |

## Spec round 2 amend 记录（2026-06-08；reviewer Spec round 1 复核后）

reviewer ACK D2/D4/D5；D1/D3/D6/D7 各 1 语义阻断，已回修：

| # | round 1 阻断 | 修订 |
|---|---|---|
| B1（D3）| "disable adapter 失败留原态" 与现有 effectful 机制冲突（`ipc_tick.rs:199` 失败 → `commit_fault(AdapterErr)` + `Disabled`）| **disable 失败 → Fault(AdapterErr) + wire `RpcStatus`（round 3 B5 收窄为 `Internal`，`AdapterError` 不存在）+ domain status，不返 Ack success**（沿现有 plan_transition/pending effectful 机制，**不**自定义 handler 扩范围）+ 补 AC 失败路径 |
| B2（D6）| auto_enable 绑定未按 resolved hardware_id 定义（hardware_id = CLI 或 `[[hardware]].first()`，`s0_daemon.rs:260`；build_adapter 取 `.first()` `:1061`；auto_enable 在 untyped `extras` `config_loader.rs:72`）| **D6 定义 resolution**：match `[[hardware]].id == resolved hardware_id` → 读该段 `auto_enable`（**加 typed `auto_enable: Option<bool>`**，缺省 true）；**无匹配 → 启动 fail-fast**（config/CLI mismatch）|
| B3（D1）| "whole-robot enable" 越过当前模型（单 RR service 做不到多 S0 fanout）| **D1 改"单 S0 daemon 实例 / selected hardware instance enable/disable"**；多 S0 / per-hardware / whole-robot fanout **推后** → `bool enabled` 不加 hardware_id 自洽 |
| B4（D7）| dry_run "would-succeed" 过度承诺（不执行 `Enable(false)` 无法知 adapter ack）| **dry_run 返 "would_accept / precondition result"**（状态机/guard/precondition 接受性，**明确不代表 adapter 会 ack**）；AC-6 锁 0 state / 0 adapter request / 0 latch + response 文案不误导 |

## What

实现 design §5.3 的 `SetEnabled` 控制 RPC —— operator 显式 **enable/disable
当前 S0 daemon / selected hardware instance**（S0 enable 状态机的首个 operator/RPC
transport；范围限定见 D1）：

- `SetEnabled(SetEnabledCommand{ meta, bool enabled })` → `Ack`（design §5.3 frozen）。
- **additive**：proto method 8→9（既有 8 method 0 改）；归 **S0 control action**
  （沿 A1 ClearEStop transport 范式，**不**走 S1，§5.5）。
- `enabled=true` → 状态机 `Event::Enable`（Safe→Ready，latch-guarded）；`enabled=false`
  → **新 `Event::Disable`**（torque-off，不清 latch）。

## 影响范围

### 必改（候选 C land 必须）

- `crates/robot-rpc/proto/robot_control.proto`：append `rpc SetEnabled` + message
  `SetEnabledCommand{ CommandMeta meta = 1; bool enabled = 2; }`（头注 method 8→9）。
- `crates/robot-msg/schemas/set_enabled.fbs`（**新**）：`SetEnabledRequest`（meta
  派生字段 + `role`〔audit〕+ `deadline_ns` + `idempotency`〔gateway 用〕+ **`dry_run`** +
  `bool enabled`）+ `SetEnabledResponse`（wire `RpcStatus`〔**复用 A1 共享 enum，
  不扩**〕+ **domain `SetEnabledStatus`**〔候选 C 自有 enum：如 `Ok` /
  `AdapterError` / `GuardRejected` / `LatchBlocked`，供审计区分，B5〕）。沿
  `clear_estop.fbs` 字段范式（`role` 仅 audit、S0 不授权）。`SetEnabledStatus` 也
  进 `schema_id`/`schema_lint` 不涉（它是 response 内 enum，非独立 schema_id）。
- `crates/robot-msg/src/schema_id.rs`：`SCHEMA_SET_ENABLED_REQUEST = 0x0000_040D` /
  `SCHEMA_SET_ENABLED_RESPONSE = 0x0000_040E`（当前 max `SCHEMA_IK_PAYLOAD=0x040C`
  之后）+ 加进 **`SCHEMA_REGISTRY`**（`(u32,&str)` 升序列表，`schema_lint` 锁唯一
  + 单调）。
- `crates/robot-rpc/src/gateway.rs` + bin：`SetEnabled` handler（auth/role/capability
  校验 + idempotency + `complete_ack_rpc` 桥）+ **第 6 条 rr_io_thread**
  （`/<robot_id>/control/set_enabled` → S0）。
- `crates/robot-s0/`：**第 2 个 rr_server_thread**（set_enabled topic，D5）+
  `ipc_thread` set_enabled RR drain + `ipc_tick` emit `Event::Enable`/新
  `Event::Disable`（effectful `Enable(false)`，**失败沿现有 `commit_fault(AdapterErr)`**
  机制，D3d/B1）+ **状态机 `Event::Disable` + transition**（state_machine.rs）+
  set_enabled action handler（含 dry_run precondition evaluator，D7/B4）+
  `config_loader.rs` 加 **typed `auto_enable: Option<bool>`**（D6/B2）+ **单一 helper
  `resolve_hardware_section(cfg, hardware_id)`**（按 resolved hardware_id match
  `[[hardware]].id`，无匹配 → fail-fast，B6）+ **`build_adapter` 与 auto-enable
  改用同一 matched section**（修现有 `.first()` 错配，`s0_daemon.rs:1061`，B6）。
- `docs/CHANGELOG.md`：登记（Q1 不加 hardware_id 故**不**触 design-freeze diff；
  SetEnabled write-RPC idempotency；auto_enable typed 字段 additive）。
- e2e：cross-process（**s0 + heartbeat 源〔s1_daemon 或 fake heartbeat〕+ gateway**，
  D8：enable-happy 观测 **Enabled.Active** 才算 success + enable deadline〔无
  heartbeat〕+ enable/disable adapter 失败→Fault + latch-blocked enable +
  latched-disable not-clear-latch + dry_run precondition + auth + auto_enable
  resolution + AC-8b adapter 同段）。

### 严禁改动（hard boundary）

- **§6.7 五条不变量**：SetEnabled **不清/不置 estop_latch**（只 ClearEStop 清）、
  enable 必经 `ready_guards_pass()`（含 `!estop_latched`）、disable 走 torque-off
  **不是** EStop、不绕 FAULT（Fault→Ready 只 ClearError）。
- **B4 frozen 8-method wire schema**：既有 8 method 字段/字段号 0 改（additive
  第 9 method）。
- **ClearEStop / ClearError 语义**：set_enabled 第 2 个 S0 RR server，**不**碰
  ClearEStop rr_server_thread / clear_estop_action（D5）。
- **S1 0 改**：SetEnabled 是 S0 控制（§5.5）。
- **`docs/00_design.md` 0 改**：SetEnabled 签名偏离（Q1 若加 hardware_id）走
  CHANGELOG。

## 关键设计决策（5 拍板项 + 2 已锁）

### D1（Q1 多 hardware）⚠ **拍板**：`bool enabled`，**不**加 hardware_id（B3 收窄）

- 倾向 **保留 frozen `bool enabled`，无 design-freeze diff**；**范围 = 单 S0 daemon
  实例 / 该 daemon 的 selected hardware instance** 的 enable/disable（**不**宣称
  whole-robot）。
- **理由**：S0 状态机是 **per-S0-daemon 单实例**（一个 `S0Mode`，**非** per-bus）；
  单 `/<robot_id>/control/set_enabled` RR service 对应**单 S0 daemon**，`bool
  enabled` = 该 daemon 的 enable 态。**区别于候选 B GetState**（读天然 per-hardware）。
- **明确推后**（B3）：**多 S0 daemon / per-hardware / whole-robot fanout + 确认**
  （需广播到多 RR service + 聚合 ack）**不在候选 C**；候选 C 只覆盖单 S0 daemon。
  正因范围限定为单 daemon，`bool enabled` 不加 hardware_id 才自洽。
- **(b) 备选**（reviewer 若要 per-hardware）：加 `hardware_id` = design-freeze diff
  （CHANGELOG），但与当前单状态机不匹配，**不推荐**。

### D2（Q2 role）= **已锁 operator+**（非开放）

- `set_enabled` capability **已映射** operator/maintainer/admin（`rpc_roles.toml:54`，
  **viewer 无**）+ `Capability::SetEnabled` 已存在（`auth.rs:66`）。
- gateway handler 做 auth/role/capability 校验（**viewer → PermissionDenied**）；
  wire `role` 字段**仅 audit**，S0 端不授权（沿 ClearEStop A1-1 拍板）。
- 候选 C = **复核确认 + e2e 锁**（viewer 拒 / operator 准），不改映射。

### D3（Q3 disable）⚠ **拍板五项一次拍清**

`SetEnabled(false)` = 新 `Event::Disable`，五项倾向：

| # | 项 | 倾向 |
|---|---|---|
| (a) | 目标态 | **→ Safe**（复用 Safe，**不**新增 `Disabled` 子态；Safe 已是 torque-off 无运动态）|
| (b) | OutputPolicy | **Disabled**（torque off；Safe 的 output policy）|
| (c) | `AdapterNormalReq::Enable(false)` | **发**（disable 是 effectful，沿 enable 上扭对称；真 torque-off）|
| (d) | adapter 失败映射（**B1 + B5 收紧**）| 沿**现有 effectful 机制**（`ipc_tick.rs:199`）：adapter `Enable(false)` ack 失败 → `commit_fault(FaultCode::AdapterErr)` + `OutputPolicy::Disabled`。wire **`RpcStatus = Internal`**（adapter ack fail = daemon 内部执行失败，**非** precondition；`RpcStatus` **无** AdapterError，不扩 A1 共享 enum）+ `SetEnabledResponse` 带 **domain `SetEnabledStatus::AdapterError`** 供审计区分 → gateway `Internal → tonic::Code::Internal`。**不返 Ack success**；**不**留原态、**不**自定义 handler（沿 plan_transition/pending，不扩范围）|
| (e) | latched-disable | **允许**（`hard_rt_ctrl:390` latch 下 `Enable(false)` 仍 `Submitted`；disable 一个 latched 机器人安全 + 显式；**不清 latch**）|

- **transition 加**：`(Enabled, Disable)`→Safe（effectful `Enable(false)`）/
  `(Ready, Disable)`→Safe；`(Safe/Fault, Disable)` = no-op（幂等，已非 enabled）。
- `SetEnabled(true)` = 现有 `Event::Enable`（Safe→Ready，latch-guarded）；`(Enabled,
  Enable)` no-op 幂等 / `(Fault, Enable)` 拒（需先 ClearError）。

### D4（Q4 路由）= **已确认 S0 control action**

- SetEnabled 改 S0 enable 态 → S0（§5.5）。gateway 第 6 条 rr_io_thread → S0；
  **S1 0 改**（不走 B4 task RR）。

### D5（Q5 S0 RR server）⚠ **拍板**：第 2 个**专用** rr_server_thread

- 倾向 **(a) 新增 set_enabled 专用 rr_server_thread**（topic
  `/<robot_id>/control/set_enabled`），**ClearEStop rr_server_thread 0 改**。
- **不选 (b) 泛化单 rr_server 多 topic**（reviewer 指出多 topic 泛化风险更高：动
  A1 已锁的 ClearEStop wire I/O 路径）。代价 = 一条多线程（沿 A1 模板复制），可控。

### D6（Q6 auto_enable 绑定）⚠ **拍板**：绑定 + **按 resolved hardware_id 定义**（B2 收紧）

- 现状 = s0_daemon **无条件**启动 auto-enable（`s0_daemon.rs:868` 不读 config）+
  config `auto_enable` 在 untyped `extras`（`config_loader.rs:72`）+ hardware_id =
  CLI 或 `[[hardware]].first().id`（`s0_daemon.rs:260`）。
- **resolution 定义（B2）**：
  1. 加 **typed `auto_enable: Option<bool>`** 到 `HardwareSection`（`config_loader.rs`，
     additive 字段，**不**再走 extras）。
  2. **单一 helper `resolve_hardware_section(cfg, hardware_id)`**（B6）：按 resolved
     `hardware_id` match `[[hardware]].id` → 返该段。startup 读该段 `auto_enable`，
     **缺省（None）= true**（既有 config 0 行为变化）。
  3. **无匹配段**（CLI `--hardware-id` 不在 config）→ **启动 fail-fast**（exit，
     config/CLI mismatch；不静默 fallback）。
- **B6 同步修 `build_adapter` 错配**：现有 `build_adapter` 在 --config 模式取
  `.first()`（`s0_daemon.rs:1061`），与 resolved hardware_id 可能不一致（CLI
  `--hardware-id right_arm` 但 adapter 按 first `left_arm`）。**`build_adapter` 改
  用同一 `resolve_hardware_section` matched section** → adapter_kind / joints /
  address / auto_enable **全来自同一段**，D1"selected hardware instance"与 D6
  自洽。
- `auto_enable=false` → 启动**不**自动 emit `Event::Enable`，留 Safe 等 operator
  `SetEnabled(true)`。让 SetEnabled 有意义（operator disable 后不被启动再 auto-enable）。
- **风险**：default-true 保既有 auto_enable 行为；`build_adapter` 改 matched section
  对单 hardware config（first == matched）0 行为变化，仅多 hardware + CLI 选非 first
  时修正错配（**新增回归须覆盖**）。

### D7（Q7 dry_run）⚠ **拍板**：**端到端**（杜绝候选 A "假 dry-run" 洞）

- `set_enabled.fbs` **带 `dry_run` wire 字段**（**不**像候选 A 那样 gateway 丢弃 →
  "flag 接受但实发动作"）。
- **dry_run 进 S0 action layer**（gateway **不**短路），S0 **read-only evaluate**：
  当前 state/guards 下 `Event::Enable`/`Event::Disable` 的 **precondition 是否被
  接受**（状态机 transition 合法 + `!estop_latched` 等 guard）→ 返
  **`would_accept` / precondition result**（B4：**明确不代表 adapter 会 ack**——
  dry_run 不执行 `AdapterNormalReq::Enable(*)`，无法预知硬件 ack）。**0 state change
  / 0 adapter request / 0 latch change**（沿 ClearEStop `evaluate_clear_estop_dry_run`
  范式，`clear_estop_action.rs:156`）。response 文案须写"precondition would accept"
  不写"would succeed"，避免误导。

### D8（B7 — enable/disable 成功 Ack point）⚠ **拍板**：Ack only after actual completes

- **采纳 reviewer 倾向 = Ack 仅在真正 enable/disable 完成后返**（**不**选
  accepted-only，避免 operator 误以为已上扭）。
- **`SetEnabled(true)` 成功 Ack** = state 已 commit **Enabled.Active** + adapter
  `Enable(true)` ack 成功。S0 action emit `Event::Enable`（Safe→Ready）后**在
  request deadline 内观测** heartbeat 驱动的 Ready→Enabled.Active（effectful
  `Enable(true)`，`state_machine.rs:580/1124`）：
  - latch/guards 不满足（Safe→Ready 拒）→ `RpcStatus::FailedPrecondition`（即时）+
    domain `GuardRejected`/`LatchBlocked`。
  - 到 Ready 但 Enabled.Active **未在 deadline 内完成**（无 heartbeat 等）→ 非 Ack；
    **`RpcStatus::DeadlineExceeded` + domain status**（与 AC-2b 统一；deadline 路径
    **不**用 Internal——Internal 专留 adapter ack fail，B7 收窄）。
  - adapter `Enable(true)` ack fail（Ready→Enabled effectful）→ `commit_fault
    (AdapterErr)` + `RpcStatus::Internal` + domain `SetEnabledStatus::AdapterError`
    （**与 disable D3d 对称**）。
  - 成功 → `Ack` + `SetEnabledStatus::Ok`。
- **`SetEnabled(false)` 成功 Ack** = state 已 commit **Safe** + adapter
  `Enable(false)` ack 成功（单 effectful transition，无 heartbeat 依赖）；失败 D3d。
- **dry_run 不变**（D7）：只 precondition `would_accept`，**不**代表 adapter ack /
  不等 Enabled.Active。
- **实现含义**：set_enabled action **非一次性**（区别 ClearEStop `try_apply` 即时）
  ——emit event 后**多 tick 观测** Enabled.Active/Safe 或 deadline/fault 才产 RR
  response（Plan 期细化观测机制 + deadline 来源 = `CommandMeta.timeout_ms` 转
  wire deadline）。
- **e2e 依赖**：enable-happy 观测 Enabled.Active **需 heartbeat 源**（Ready→Enabled
  靠 heartbeat）→ SetEnabled e2e 起 **s0 + heartbeat 源（s1_daemon 或 fake
  heartbeat）+ gateway**（区别候选 B gateway-only）。

### 其它（非拍板）

- **idempotency**（§6.7 #5 write RPC）：SetEnabled 走 gateway `IdempotencyCache`
  （`idempotency_key` fingerprint，沿 ClearEStop）；enable/disable 本身幂等，cache
  防重放副作用歧义。

## Acceptance Criteria（round 4）

- **AC-1**：proto additive —— append `SetEnabled` + `SetEnabledCommand`（method
  8→9）；既有 8 method diff=0；fbs 0 退化（`schema_lint` + `cargo test -p
  robot-msg`）；既有 RPC e2e（`task_rpc_e2e` / `state_rpc_e2e`）0 退化。
- **AC-2（enable happy，D8）**：Safe + guards-ok + **heartbeat 源在跑** →
  `SetEnabled(true)` → 观测状态机到 **Enabled.Active**（Safe→Ready→Enabled.Active）+
  adapter `Enable(true)` ack → **才返 `Ack`**（`SetEnabledStatus::Ok`）。**RPC success
  ⟺ 观测到 Enabled.Active**（非 Ready-accepted）。
- **AC-2b（enable deadline，D8）**：到 Ready 但 deadline 内无 heartbeat/Enabled.Active
  → 非 Ack；`RpcStatus::DeadlineExceeded` + domain status。
- **AC-2c（enable adapter 失败，D8，与 AC-3b 对称）**：Ready→Enabled 的 adapter
  `Enable(true)` ack 失败 → `commit_fault(AdapterErr)` + wire `RpcStatus::Internal` +
  domain `SetEnabledStatus::AdapterError`。
- **AC-3（disable happy + D3）**：Enabled → `SetEnabled(false)` → `Event::Disable`
  → adapter `Enable(false)` ack → Safe（OutputPolicy Disabled）；`Ack`；**estop_latch
  文件 0 改**（disable 不清/不置 latch）。
- **AC-3b（disable adapter 失败，D3d / B1 / B5）**：Enabled → `SetEnabled(false)` →
  adapter `Enable(false)` **ack 失败** → `commit_fault(AdapterErr)` + `Disabled` →
  **wire `RpcStatus::Internal` + domain `SetEnabledStatus::AdapterError`（非 Ack
  success）**→ gateway `tonic::Code::Internal`；state = Fault(AdapterErr)。
- **AC-4（§6.7 #3 latch-blocked enable）**：estop_latched=true → `SetEnabled(true)`
  → `ready_guards_pass` 拒 → 留 Safe → 返 domain status（FailedPrecondition 类）；
  **latch 文件不变**。
- **AC-5（latched disable 允许，D3e）**：estop_latched=true + Enabled →
  `SetEnabled(false)` → Safe（torque-off）；**latch 仍 true**（不清）。
- **AC-6（dry_run precondition，D7 / B4）**：`SetEnabled(true/false, dry_run=true)`
  → S0 evaluate-only → **`would_accept` / precondition result**（**不**宣称 adapter
  会 ack）；**state + adapter request + latch 全 0 change**；反例：dry_run **不**被
  gateway 丢弃（wire 带 dry_run）；response 文案"precondition would accept"不写
  "would succeed"。
- **AC-7（auth，D2）**：viewer `SetEnabled` → PermissionDenied；operator/maintainer
  → 准。
- **AC-8（auto_enable resolution，D6 / B2）**：(i) `auto_enable=false` 的 resolved
  hardware 段 → 启动留 Safe（不自动 enable）；(ii) 缺省/true → 启动 auto-enable
  （既有行为）；(iii) CLI `--hardware-id` 无匹配 `[[hardware]].id` → **启动 fail-fast**。
- **AC-8b（adapter section 一致，B6）**：多 hardware config + CLI `--hardware-id`
  选**非 first** 段 → `build_adapter` 的 adapter_kind / joints / address **与**
  auto_enable **来自同一 matched 段**（非 `.first()` 错配）；单 hardware（first ==
  matched）0 行为变化。
- **AC-9（Fault 拒 enable）**：Fault → `SetEnabled(true)` → 拒（需先 ClearError）。
- **AC-Q**：fmt + clippy(-D warnings) + `cargo test --workspace` + 4 既有 smoke 0
  退化 + cross-process e2e + cargo deny baseline + fbs schema_lint。

## hard boundaries（实施期不破）

§6.7 全保（不清/不置 latch、enable 经 `!estop_latched` guard、disable≠EStop、不绕
FAULT）· B4 frozen 8-method 0 改（additive 第 9）· ClearEStop/ClearError rr_server +
action 0 改（D5 专用第 2 server）· S1 0 改 · design.md 0 改（Q1 走 CHANGELOG）·
A→B→C→D→E 硬序（候选 D EStop 不提前）。

## 后续（RIPER）

Spec round 1 → reviewer round 1（D2/D4/D5 ACK + 4 语义阻断 B1-B4）✅ →
Spec round 2 → reviewer round 2（2 执行级阻断 B5/B6）✅ →
Spec round 3 → reviewer round 3（B7 enable Ack point 缺口）✅ →
Spec round 4 → reviewer round 4 **ACK 全 D1-D8**（+ 非阻断 D8 deadline 去"或
Internal"，`425dd62`）✅ → **Plan outline draft v1（本文 §Plan）→ 待 reviewer
Plan round 复核** → 实施。**Plan ACK 前不开实现。**

## Plan（outline draft v1）

> reviewer ACK Spec D1-D8 → 授权 Plan outline draft v1（**不直接实现**）。本 outline
> 给 commit 拆分 + 关键 Plan 拍板项；待 reviewer Plan round 收紧后定稿。

### P.0 工作基线

- **HEAD = `425dd62`**（Spec round 4 final）。候选 B 完结 = `b795339`。
- 全程 nightly；每 commit 前 `cargo fmt --all` + `cargo clippy --workspace
  --all-targets -- -D warnings` 零警告。
- **0 新依赖**（proto/tonic/iceoryx2/flatbuffers/serde/toml 全已在）。

### P.1 commit 节奏（5 feat + 1 summary，draft）

| 段 | 内容 | 主要文件 |
|---|---|---|
| (1/5) | **schema 链 atomic**：proto `SetEnabled`+`SetEnabledCommand`（method 8→9）+ `set_enabled.fbs`（`SetEnabledRequest`/`SetEnabledResponse`/`SetEnabledStatus`）+ `schema_id` `0x040D/0x040E` + `SCHEMA_REGISTRY` + `build_helpers` + `markers`（**单 commit 避中间编译 fail**，沿候选 1/2）| proto + robot-msg |
| (2/5) | **S0 config + 状态机**：`config_loader` typed `auto_enable` + `resolve_hardware_section` helper + `build_adapter` 改 matched section（B6）+ `state_machine` `Event::Disable` + transitions（Enabled/Ready→Safe effectful `Enable(false)`）| robot-s0 config/state_machine |
| (3/5) | **S0 RR server + action**：第 2 个 `rr_server_thread`（set_enabled topic，D5）+ `ipc_thread` set_enabled RR drain + **set_enabled action（多 tick 观测 Enabled.Active/Safe + deadline + fault + dry_run evaluator，D7/D8）** + `s0_daemon` wiring（2nd rr_server + auto_enable 绑定 D6）| robot-s0 rr_server/ipc/bin |
| (4/5) | **gateway**：`SetEnabled` tonic handler + 第 6 条 rr_io_thread + auth/role/capability + idempotency + `complete_ack_rpc` 桥 | robot-rpc gateway/bin |
| (5/5) | **e2e + summary**：cross-process（s0 + heartbeat 源 + gateway）覆盖 AC-2/2b/2c/3/3b/4/5/6/7/8/8b/9 + summary + CHANGELOG + CLAUDE + README | robot-rpc tests + docs |

### P.2 工作流（draft；Plan round 细化）

- 顺序硬约束：(1/5) schema 先（下游编译依赖）→ (2/5) S0 核心 → (3/5) S0 RR+action
  → (4/5) gateway → (5/5) e2e。每段独立过 Quality Gate（fmt + clippy + 相关 unit
  + 既有 0 退化）。
- (3/5) 是**最重难点**（D-P1 观测机制）；(2/5) build_adapter 改 matched section
  须**既有 adapter 路径 0 退化回归**。

### P.3 Plan 拍板项（reviewer Plan round 决定）

- **D-P1（enable Ack 观测机制）⚠ 核心**：set_enabled action 须在 emit `Event::Enable`
  后**延迟 RR response** 直到 Enabled.Active（success）/ Safe→Ready 拒（FailedPrecond）/
  deadline（DeadlineExceeded）/ adapter fault（Internal+AdapterError）。ipc_thread
  已有 `IpcRetryState`/pending（effectful Ready→Enabled 等 `Enable(true)` ack）——
  方案倾向**复用 pending 机制 + 加 "pending RR response" 在目标态/deadline/fault
  达成时 resolve**（区别 ClearEStop one-shot）。具体观测点 + RR response 何时发由
  Plan round 拍。
- **D-P2（deadline 源/追踪）**：`CommandMeta.timeout_ms` → gateway 转 wire
  `deadline_ns`（沿 ClearEStop）→ S0 action 用 `clock.now() > deadline` 判
  DeadlineExceeded。
- **D-P3（e2e heartbeat 源）**：`s1_daemon`（真，沿 task_rpc_e2e）vs fake heartbeat
  publisher（轻、确定）。倾向 fake heartbeat（隔离 S1，专测 SetEnabled）；待拍。
- **D-P4（idempotency）**：SetEnabled write RPC → gateway `IdempotencyCache`
  （沿 ClearEStop）？倾向 yes（§6.7 #5），但 enable/disable 天然幂等——cache 必要性
  待拍。
- **D-P5（build_adapter 重构风险）**：`.first()`→`resolve_hardware_section` 改动
  adapter 构造路径（所有 config 走）；单 hardware（first==matched）0 变化，多
  hardware + CLI 选非 first 才变 → 回归须覆盖既有 smoke（crash_restart /
  scripted / ik 用单/多 hardware config）。

### P.4 Plan-Execute diff 登记（实施期填）

（占位：沿候选 A/B §P.4 体例。）
