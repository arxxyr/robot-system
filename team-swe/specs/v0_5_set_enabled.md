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
  domain `SetEnabledStatus::AdapterError`。**验证 = (3/5) 集成 unit
  `pending_adapter_fault_resolves_internal_adapter_error`（fault-inject mock
  adapter），非 cross-process e2e（PB10：mock_hard_rt 无可控 ack-fail 开关）**。
- **AC-3（disable happy + D3）**：Enabled → `SetEnabled(false)` → `Event::Disable`
  → adapter `Enable(false)` ack → Safe（OutputPolicy Disabled）；`Ack`；**estop_latch
  文件 0 改**（disable 不清/不置 latch）。
- **AC-3b（disable adapter 失败，D3d / B1 / B5）**：Enabled → `SetEnabled(false)` →
  adapter `Enable(false)` **ack 失败** → `commit_fault(AdapterErr)` + `Disabled` →
  **wire `RpcStatus::Internal` + domain `SetEnabledStatus::AdapterError`（非 Ack
  success）**→ gateway `tonic::Code::Internal`；state = Fault(AdapterErr)。**验证同
  AC-2c = (3/5) 集成 unit（PB10），非 cross-process e2e**。
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
Internal"，`425dd62`）✅ → Plan outline draft v1 → Plan round 1（PB1-3）→ Plan
round 2（PB4-6）→ **Plan round 3 executable（本文 §Plan，PB7 展开 P.2.1-2.5）→ 待
reviewer Plan Approved** → 实施。**Plan Approved 前不开实现。**

## Plan（outline draft v1）

> reviewer ACK Spec D1-D8 → 授权 Plan outline draft v1（**不直接实现**）。本 outline
> 给 commit 拆分 + 关键 Plan 拍板项；待 reviewer Plan round 收紧后定稿。

#### Plan round 1 amend 记录（2026-06-08；reviewer Plan round 1 复核后）

reviewer 3 阻断 + 1 非阻断，已回修：

| # | 阻断 | 修订 |
|---|---|---|
| PB1 | (1/5) append proto service method 强制 GatewayHandler impl set_enabled（否则 workspace 不编译，候选 B PE2 已踩）| **(1/5) 只 land robot-msg fbs schema 链**（不含 proto method）；**proto `SetEnabled` + `SetEnabledCommand` 挪到 (4/5) 跟 gateway handler atomic land**（proto+impl 同 commit，**无需 stub**）。S0 (2/5)(3/5) 只用 fbs，不依赖 proto |
| PB2 | D-P1 pending RR response 不够执行级 | D-P1 展开 6 条：single-flight + 并发拒 / 与 `IpcRetryState` 所有权 / EStop·fault·shutdown resolve 不悬挂 / dry_run 不占 pending不 emit / deadline 边界（response vs 副作用）|
| PB3 | D-P4 idempotency 不能待拍 | **锁定 SetEnabled write RPC → gateway `IdempotencyCache`**（§6.7 #5）+ replay = cached Ack；**Ack 语义定义**：SetEnabled 返**通用 Ack**（frozen §5.3），`cleared_at_ns` = **set-enabled 完成时刻**（Enabled.Active/Safe commit 的 daemon clock.now()；dry_run → would-act eval；status≠Ok → 0），名保留语义文档化 |
| 非阻断 | D-P3 fake heartbeat | fake heartbeat **走真实 heartbeat topic `/<robot_id>/s1/heartbeat`**（S0 订阅，`s0_daemon.rs:354`）驱动真 S0 heartbeat 路径 → Ready→Enabled，**不**直接调 S0 内部函数 |

#### Plan round 2 amend 记录（2026-06-08；reviewer Plan round 2 复核后）

PB1/PB3 ACK；D-P1 又 3 执行级阻断（RpcStatus 语义），已回修（**0 扩 A1 共享 enum**）：

| # | 阻断 | 修订 |
|---|---|---|
| PB4 | 并发第二个 distinct 写 `Aborted` 冲突锁定语义（`clear_estop.fbs:111`：`ResourceExhausted`=live 单飞、`Aborted`=EStop preempt）| D-P1 条 1：并发第二个（同/异 key）→ **`ResourceExhausted`**（AlreadyInProgress），非 Aborted |
| PB5 | Stop/shutdown 写 `Unavailable`，但 `RpcStatus` 无此 variant（B5 不扩 A1 enum）| D-P1 条 3：Stop/shutdown preempt → **`Aborted`**（复用 in-flight-preempt 语义，domain `SetEnabledStatus` 区分 EStop vs shutdown）；**不** append Unavailable |
| PB6 | 同 key in-flight "gateway replay 不进 S0" 不成立（`decide` in-flight 仍 `FreshInsert`，`idempotency.rs:303`）| D-P1 条 1 + D-P4：**replay 只在第一个 commit 后**；in-flight 同 key 仍 FreshInsert→forward→**S0 single-flight ResourceExhausted**；**不**加 gateway in-flight guard（S0 single-flight 即防线）|

#### Plan round 3 amend 记录（2026-06-08；reviewer Plan round 2 复核后）

reviewer：D-P1/PB4-6 ACK，但 Plan 停在 outline 级，未 Approved。**展开 P.2 到
文件/函数/测试名级**（1 阻断）+ HEAD baseline 同步（1 非阻断）：

| # | 阻断 | 修订 |
|---|---|---|
| PB7 | Plan 仍 outline（"Plan round 定字段"），未到可执行粒度 | **P.2 重写为 P.2.1-P.2.5 executable**：每段补 set_enabled.fbs 字段表 / schema_id 常量 / markers / build_helpers / `resolve_hardware_section` 签名 / `Event::Disable` transition / `PendingSetEnabled` 字段 + single-flight + resolve 分支 / `Method::SetEnabled` + `CachedResponse::SetEnabled` / gateway `set_enabled` handler+mapper+rr thread / e2e case 名（含 ResourceExhausted/Aborted/DeadlineExceeded/adapter-fail/dry-run/auto_enable/viewer 拒）+ 锁测名 |
| 非阻断 | P.0 HEAD `425dd62` stale | HEAD = 本 Plan round 3 commit，Execute 前同步实际 HEAD |

#### Plan round 4 amend 记录（2026-06-09；reviewer Plan round 3 复核后）

reviewer：P.2 接近可执行，3 执行级阻断（response/pending correlation + e2e fault
触发），已回修：

| # | 阻断 | 修订 |
|---|---|---|
| PB8 | `SetEnabledResponse` 无 `request_id`（丢 RR correlation + gateway mismatch 防线，沿 `clear_estop.fbs:177`）| (1/5) `SetEnabledResponse` 加 **`request_id:string`** + **`error_msg:string`**（非 Ok human reason）；`build_set_enabled_response(request_id, …, error_msg)` 同步 |
| PB9 | `PendingSetEnabled` 字段/reply 机制不够执行级 | (3/5) 字段定死 `{ request_id, deadline_ns, target: PendingTarget{EnabledActive\|Safe}, requested_enabled }`；**reply = 复用 set_enabled RR response channel + request_id 路由**（非 per-pending oneshot，沿 `ClearEStopRrResponse{request_id}` `ipc_thread.rs:123`）；resolve 构造 `SetEnabledRrResponse{request_id, payload}` |
| PB10 | e2e `enable_adapter_fail` 无可执行 fault 触发（mock_hard_rt enable_rt 只在 latch/disconnect 失败）| **adapter-fail（AC-2c/AC-3b）移 (3/5) 集成 unit** `pending_adapter_fault_resolves_internal_adapter_error`（fault-inject mock adapter）；(5/5) cross-process e2e 去掉 2 个 adapter-fail case |

#### Plan round 5 amend 记录（2026-06-09；reviewer Plan round 4 复核后）

reviewer：PB8-10 方向对，3 执行级阻断（domain payload / 防线 / stale），已回修：

| # | 阻断 | 修订 |
|---|---|---|
| PB11 | `SetEnabledRrPayload` 无 `DeadlineExceeded` variant，但 deadline 路径要 `RpcStatus::DeadlineExceeded` → 无可编码 domain | (1/5) `SetEnabledStatus += DeadlineExceeded=9`；(3/5) `SetEnabledRrPayload += DeadlineExceeded` + 补**全 payload→wire encode 映射表**（含 `DeadlineExceeded → RpcStatus::DeadlineExceeded + domain DeadlineExceeded`）|
| PB12 | (4/5) handler 缺 ClearEStop 的 `response.request_id == request_id` 防线（mismatch → Internal + 不缓存，`gateway.rs:698`）| (4/5) handler 步加 mismatch 防线 + unit `set_enabled_wire_response_request_id_mismatch_returns_internal_and_does_not_cache` |
| PB13 | P.3 残留旧 `PendingSetEnabled{ deadline, target, rr_reply }` + "Plan round 定字段"，与 PB9/P.2.3 矛盾 | P.3 D-P1 观测落点改"**以 P.2.3 为执行权威**"，同步 PB9 字段 + response channel 机制 |

### P.0 工作基线

- **HEAD = 本 Plan round 3 amend land 的 commit**（**Execute (1/5) 前同步实际 HEAD**，
  不写 stale hash；非阻断）。Spec final = `425dd62`；候选 B 完结 = `b795339`。
- 全程 nightly；每 commit 前 `cargo fmt --all` + `cargo clippy --workspace
  --all-targets -- -D warnings` 零警告。
- **0 新依赖**（proto/tonic/iceoryx2/flatbuffers/serde/toml 全已在）。

### P.1 commit 节奏（5 feat + 1 summary，draft）

| 段 | 内容 | 主要文件 |
|---|---|---|
| (1/5) | **robot-msg fbs schema 链 atomic（PB1：不含 proto method）**：`set_enabled.fbs`（`SetEnabledRequest`/`SetEnabledResponse`/`SetEnabledStatus`）+ `schema_id` `0x040D/0x040E` + `SCHEMA_REGISTRY` + `build_helpers` + `markers`（**单 commit 避中间编译 fail**，沿候选 1/2）。**proto method 不在此**（否则强制 GatewayHandler trait，候选 B PE2）| robot-msg |
| (2/5) | **S0 config + 状态机**：`config_loader` typed `auto_enable` + `resolve_hardware_section` helper + `build_adapter` 改 matched section（B6）+ `state_machine` `Event::Disable` + transitions（Enabled/Ready→Safe effectful `Enable(false)`）| robot-s0 config/state_machine |
| (3/5) | **S0 RR server + action**：第 2 个 `rr_server_thread`（set_enabled topic，D5）+ `ipc_thread` set_enabled RR drain + **set_enabled action（多 tick 观测 Enabled.Active/Safe + deadline + fault + dry_run evaluator，D7/D8）** + `s0_daemon` wiring（2nd rr_server + auto_enable 绑定 D6）| robot-s0 rr_server/ipc/bin |
| (4/5) | **gateway（PB1：proto method 在此 atomic）**：proto append `SetEnabled` + `SetEnabledCommand`（method 8→9）+ `SetEnabled` tonic handler（proto+impl 同 commit，无需 stub）+ 第 6 条 rr_io_thread + auth/role/capability + **idempotency（`IdempotencyCache`，PB3）** + `complete_ack_rpc` 桥 | proto + robot-rpc gateway/bin |
| (5/5) | **e2e + summary**：cross-process（s0 + heartbeat 源 + gateway）覆盖 AC-2/2b/2c/3/3b/4/5/6/7/8/8b/9 + summary + CHANGELOG + CLAUDE + README | robot-rpc tests + docs |

### P.2 工作流（executable，按 commit 顺序）

顺序硬约束：(1/5) fbs 先（S0 RR drain 编译依赖）→ (2/5) S0 核心 → (3/5) S0 RR+
action → (4/5) proto + gateway → (5/5) e2e。每段独立过 Quality Gate。命名沿
ClearEStop（A1）先例。

#### P.2.1 [commit (1/5)] robot-msg fbs schema 链（atomic）

- **`crates/robot-msg/schemas/set_enabled.fbs`**（新）：
  - `table SetEnabledRequest { request_id:string; idempotency_key:string;
    source:string; role:RpcRole=Unknown; enabled:bool=false; dry_run:bool=false;
    deadline_ns:ulong; audit_reason:string; stamp_ns:ulong; }`（复用 `clear_estop.fbs`
    的 `RpcRole`；drop `confirmation`，加 `enabled`）。
  - `enum SetEnabledStatus:ubyte { Unknown=0; Ok=1; GuardRejected=2; LatchBlocked=3;
    AdapterError=4; AlreadyInProgress=5; PreemptedByEStop=6; PreemptedByShutdown=7;
    WouldAccept=8; DeadlineExceeded=9; }`（domain 审计 enum；wire `RpcStatus` 另带，
    PB3/PB5；**`DeadlineExceeded=9` PB11**）。
  - `table SetEnabledResponse { request_id:string; status:RpcStatus=Unknown;
    domain:SetEnabledStatus=Unknown; completed_at_ns:ulong; error_msg:string; }`
    （**PB8**：`request_id` 与 request 一致——gateway correlate pending response +
    mismatch 防线，沿 `clear_estop.fbs:177`；`error_msg` 非 Ok 时携带 human reason
    供 gateway 直接转 tonic message，不靠 domain 合成）。
- **`schema_id.rs`**：`SCHEMA_SET_ENABLED_REQUEST: u32 = 0x0000_040D;` +
  `SCHEMA_SET_ENABLED_RESPONSE: u32 = 0x0000_040E;` + `SCHEMA_REGISTRY` 末尾 append
  `(SCHEMA_SET_ENABLED_REQUEST, "robot.msg.SetEnabledRequest")` /
  `(…RESPONSE, "robot.msg.SetEnabledResponse")`（升序）。
- **`markers.rs`**：`pub struct SetEnabledRequest;` / `pub struct SetEnabledResponse;`
  + `impl BodyMarker`（`SCHEMA_ID` / `SCHEMA_NAME` / `Root`）。
- **`build_helpers.rs`**：`build_set_enabled_request(request_id, idempotency_key,
  source, role, enabled, dry_run, deadline_ns, audit_reason, stamp_ns) -> Vec<u8>` +
  `build_set_enabled_response(request_id, status, domain, completed_at_ns, error_msg)
  -> Vec<u8>`（PB8）。
- **锁测**：`cargo run -p robot-msg --bin schema_lint`（升序+唯一）+ `cargo test -p
  robot-msg`（registry roundtrip）+ build_helpers roundtrip unit
  `set_enabled_request_roundtrip` / `set_enabled_response_roundtrip`。
- **Quality Gate**：fmt + clippy + robot-msg test + schema_lint；**proto 不动**
  （PB1）→ 下游 gateway/s0 不强制改。

#### P.2.2 [commit (2/5)] S0 config + 状态机

- **`config_loader.rs`**：`HardwareSection` 加 `#[serde(default)] pub auto_enable:
  Option<bool>`（typed，B2）+ `pub fn resolve_hardware_section<'a>(cfg:
  &'a RobotConfigToml, hardware_id: &str) -> Option<&'a HardwareSection>`（match
  `.id == hardware_id`，B6）。
- **`s0_daemon.rs`**：`build_adapter` 与 auto_enable 改用 `resolve_hardware_section`
  （非 `.first()`，B6）；无匹配 → `std::process::exit` fail-fast。
- **`state_machine.rs`**：`Event` enum 加 `Disable`；transition `(Enabled, Disable)
  → Safe` / `(Ready, Disable) → Safe`（effectful，policy `OutputPolicy::Disabled`）；
  `(Safe/Fault, Disable)` no-op（幂等）。
- **unit**：`disable_from_enabled_to_safe` / `disable_from_ready_to_safe` /
  `disable_from_safe_noop` / `resolve_hardware_section_by_id` /
  `resolve_hardware_section_no_match_none` / `auto_enable_typed_default_true`。
- **回归（D-P5）**：单 hardware config（first==matched）adapter 构造 0 变化——
  既有 state_machine + s0 unit 全过。

#### P.2.3 [commit (3/5)] S0 RR server + set_enabled action（**最重难点 D-P1**）

- **`set_enabled_rr_server.rs`**（新，**copy** rr_server_thread.rs 范式，SetEnabled-
  typed；**ClearEStop `rr_server_thread.rs` 0 改**，D5）：`spawn_set_enabled_rr_server_
  thread(config)`，topic `/<robot_id>/control/set_enabled`，decode `markers::
  SetEnabledRequest` → forward `SetEnabledRrRequest` → encode `SetEnabledResponse`。
- **`ipc_thread.rs`**：`pub struct SetEnabledRrRequest { request_id, enabled,
  dry_run, deadline_ns, role, … }` + `pub struct SetEnabledRrResponse { request_id,
  payload: SetEnabledRrPayload }` + `pub enum SetEnabledRrPayload { Enabled{
  completed_at_ns }, Disabled{ completed_at_ns }, WouldAccept{ at_ns }, GuardRejected
  { actual }, LatchBlocked, AdapterError, AlreadyInProgress, PreemptedByEStop,
  PreemptedByShutdown, DeadlineExceeded }`（沿 `ClearEStopRrPayload` 范式；
  **`DeadlineExceeded` PB11**）。**encode 映射**（payload → wire）：`Enabled`/`Disabled`
  → `RpcStatus::Ok` + domain `Ok`；`WouldAccept` → `Ok` + domain `WouldAccept`；
  `GuardRejected`/`LatchBlocked` → `FailedPrecondition` + domain 对应；`AdapterError`
  → `Internal` + domain `AdapterError`；`AlreadyInProgress` → `ResourceExhausted`；
  `PreemptedByEStop`/`PreemptedByShutdown` → `Aborted` + domain 对应；**`DeadlineExceeded`
  → `RpcStatus::DeadlineExceeded` + domain `DeadlineExceeded`（PB11）**。
- **`set_enabled_action.rs`**（新，沿 `clear_estop_action.rs`）：`evaluate_set_enabled
  _dry_run(state, guards, enabled) -> SetEnabledDryRunOutcome`（D7 precondition-only）。
- **D-P1 pending 观测**（ipc_thread，**PB9 字段定死**）：
  `ipc_thread` 持 `Option<PendingSetEnabled>` +
  ```rust
  pub struct PendingSetEnabled {
      pub request_id: String,        // resolve 时回填 SetEnabledRrResponse（rr_server 按此路由，rr_server_thread.rs:558）
      pub deadline_ns: u64,          // CommandMeta.timeout_ms → wire deadline_ns（D-P2）
      pub target: PendingTarget,     // enum { EnabledActive, Safe }（enable→Active / disable→Safe）
      pub requested_enabled: bool,   // 原 SetEnabledRequest.enabled（audit + 对称性）
  }
  ```
  **reply 机制 = 复用 set_enabled RR response channel + request_id 路由**（非 per-
  pending oneshot；沿 `ClearEStopRrResponse{request_id}` 范式，`ipc_thread.rs:123`）：
  resolve 时 ipc_thread 发 `SetEnabledRrResponse{ request_id, payload }` 到
  set_enabled `rr_response_tx` → rr_server 按 request_id 找 active 回 wire。
- **single-flight check** = `self.pending_set_enabled.is_some()` → 新请求即拒
  `SetEnabledRrPayload::AlreadyInProgress`（条 1，不入 pending）。
- **resolve 分支**（每 tick check）：commit 到 `target` → `Enabled/Disabled
  {completed_at_ns = clock.now()}` ；EStop → `PreemptedByEStop` / Stop·shutdown →
  `PreemptedByShutdown` / fault commit(AdapterErr) → `AdapterError`（条 3）；
  `clock.now() > deadline_ns` → `DeadlineExceeded`（条 5，**不**回滚副作用）。每
  resolve 后 `pending_set_enabled = None`（释放 single-flight）。
- **`s0_daemon.rs`**：spawn 第 2 rr_server（set_enabled）+ 接 RR channels 到 ipc_thread。
- **unit**：`set_enabled_dry_run_precondition_only` / `pending_single_flight_rejects
  _second` / `pending_resolves_on_enabled_active` / `pending_preempt_estop_resolves
  _aborted` / `pending_deadline_resolves` / `disable_pending_resolves_on_safe` /
  **`pending_adapter_fault_resolves_internal_adapter_error`**（PB10：用 fault-inject
  mock adapter〔`enable_rt`/`Enable(*)` 返 `Err`〕→ state commit Fault(AdapterErr) →
  pending resolve `AdapterError`；覆盖 AC-2c/AC-3b 的 adapter-fail 路径，**不**走
  cross-process e2e〔mock_hard_rt 无 connected+non-latched 的可控 ack-fail 开关〕）。

#### P.2.4 [commit (4/5)] proto method + gateway（atomic，PB1）

- **`robot_control.proto`**：append `rpc SetEnabled (SetEnabledCommand) returns
  (Ack);`（service 末尾，method 8→9）+ `message SetEnabledCommand { CommandMeta
  meta=1; bool enabled=2; }`（头注 8→9）。
- **`idempotency.rs`**：`Method::SetEnabled`（as_str `"set_enabled"`）+
  `CachedResponse::SetEnabled { completed_at_ns: u64, dry_run: bool }`（沿
  `CachedResponse::ClearEStop`）。
- **`gateway.rs`** + **新 `set_enabled.rs`**（handler 纯函数，沿 `clear_estop.rs`）：
  `GatewayHandlerState` 加 `rr_forward_tx_set_enabled: Sender<RrForwardRequest>`；
  `async fn set_enabled(&self, req) -> Result<Response<Ack>, Status>`（prost trait
  method）；handler 步：`validate_request_id` → auth `lookup_token` → authz
  `Capability::SetEnabled`（viewer→PermissionDenied）→ idempotency `decide`
  （Method::SetEnabled）→ deadline → build wire `SetEnabledRequest` → forward via
  `spawn_rr_io_thread`（topic set_enabled，`client_factory_for::<markers::
  SetEnabledRequest, markers::SetEnabledResponse>`）→ await oneshot → decode
  `SetEnabledResponse` → **校验 `response.request_id == validated.request_id`，
  mismatch → `tonic::Code::Internal` + `InsertHandle` drop 不 commit cache（沿
  ClearEStop "Step 11.5" 防线，`gateway.rs:698`，PB12）** → `rpc_status_to_tonic_code`
  （**复用**，无新 mapper）→ Ok 路径 commit cache（非 Ok drop handle 不缓存）。
- **`bin/robot_rpc_gateway.rs`**：起第 6 rr_io_thread（set_enabled topic）。
- **unit**：`set_enabled_viewer_denied` / `set_enabled_status_mapping`（复用
  rpc_status mapper 覆盖 ResourceExhausted/Aborted/Internal/DeadlineExceeded）/
  `cached_response_set_enabled_roundtrip` /
  **`set_enabled_wire_response_request_id_mismatch_returns_internal_and_does_not_cache`**
  （PB12 防线）。

#### P.2.5 [commit (5/5)] cross-process e2e + summary

- **`tests/set_enabled_e2e.rs`**（新，沿 `state_rpc_e2e` harness + s0/fake-heartbeat）：
  起 **s0_daemon + fake heartbeat publisher（`/<robot_id>/s1/heartbeat`，D-P3）+
  gateway**。
- **case 名**（cross-process）：`enable_happy_observes_enabled_active`（AC-2）/
  `enable_deadline_no_heartbeat_returns_deadline_exceeded`（AC-2b）/ `disable_happy
  _to_safe_no_latch_change`（AC-3）/ `enable_latch_blocked_failed_precondition`
  （AC-4）/ `latched_disable_allowed_no_clear_latch`（AC-5）/ `dry_run_precondition
  _no_side_effect`（AC-6）/ `viewer_denied`（AC-7）/ `auto_enable_false_stays_safe`
  + `cli_hardware_id_no_match_fail_fast`（AC-8/8b）/ `concurrent_second_resource
  _exhausted`（D-P1 条 1）/ `fault_rejects_enable`（AC-9）。
  - **AC-2c / AC-3b（adapter-fail）不在此**（PB10）：mock_hard_rt 无 connected+
    non-latched 可控 ack-fail 开关 → 由 **(3/5) `pending_adapter_fault_resolves
    _internal_adapter_error` 集成 unit** 覆盖（fault-inject mock adapter）。
- **summary** + CHANGELOG（design §5.3 SetEnabled 实现，Q1 不加 hardware_id 不触
  design-freeze diff；auto_enable typed additive）+ CLAUDE + handoffs README。
- **Quality Gate（候选 C 完结）**：fmt + clippy + `cargo test --workspace` + **4
  既有 smoke 0 退化**（含 D-P5 build_adapter 回归）+ cargo deny baseline +
  schema_lint。

### P.3 Plan 拍板项（reviewer Plan round 决定）

- **D-P1（enable Ack 观测机制）⚠ 核心（PB2 执行级展开）**：set_enabled action emit
  `Event::Enable` 后**延迟 RR response** 直到目标态/deadline/fault。**6 条须拍清**：
  1. **single-flight（PB4/PB6 收紧）**：S0 同时只允许 **1 个** pending SetEnabled
     观测。**任何并发第二个**（同/异 key）在 in-flight 窗口 → 即时拒
     **`RpcStatus::ResourceExhausted`**（= `AlreadyInProgress` 锁定语义，
     `clear_estop.fbs:111`；**非** Aborted）。**idempotency replay 只在第一个 commit
     后成立**：同 key 在 in-flight 窗口 `IdempotencyCache::decide` 仍返 `FreshInsert`
     （`idempotency.rs:303`，未 commit 无 entry）→ forward 到 S0 → S0 single-flight
     拒 ResourceExhausted；同 key **commit 后** → cache `Replay`（cached Ack）。
     **不**新增 gateway in-flight guard——**S0 single-flight 即 in-flight 防线**。
  2. **与 `IpcRetryState`/pending 所有权**：现有 pending 处理 effectful adapter ack
     （Ready→Enabled 等 `Enable(true)`）。set_enabled 的 "pending RR response" **独立
     于** IpcRetryState 但**观测同一状态机**：RR response 持有直到 state commit
     Enabled.Active/Safe（读 commit 结果），**不**抢 IpcRetryState 所有权。
  3. **preempt resolve（不悬挂，PB5 收紧）**：pending 期间 EStop → resolve
     **`Aborted`**（PreemptedByEmergency，`clear_estop.fbs:111`）；Stop/shutdown
     （daemon 关停）→ resolve **`Aborted`**（in-flight 被关停中止；**`RpcStatus` 无
     `Unavailable`** 故复用 `Aborted` 的 in-flight-preempt 语义，domain
     `SetEnabledStatus` 区分 EStop vs shutdown）；adapter fault（commit_fault
     AdapterErr）→ resolve `Internal` + domain `AdapterError`。**任何终态/preempt
     事件必 resolve，绝不悬挂**。
  4. **dry_run**：**不占 pending、不 emit event**（即时 precondition evaluate +
     respond，D7）。
  5. **deadline 边界**：`clock.now() > deadline` → RR response `DeadlineExceeded`；
     **状态机 + adapter 副作用按自然进程继续**（deadline 只界定 **RPC response**，
     **不**回滚已发的 `Event::Enable` / in-flight adapter ack——enable 可能 deadline
     后经 heartbeat 仍完成；沿 ClearEStop deadline+grace 精神）。
  6. **success**：state commit Enabled.Active（enable）/ Safe（disable）+ adapter ack
     → `Ack`（`cleared_at_ns` = commit 时刻，D-P4）+ domain `Ok`。
  - **观测落点字段 + reply 机制见 §P.2.3**（`PendingSetEnabled{ request_id,
    deadline_ns, target: PendingTarget, requested_enabled }` + 复用 set_enabled RR
    response channel + request_id 路由；PB9/PB13——**以 P.2.3 为执行权威**，本 D-P1
    只锁语义口径）。
- **D-P2（deadline 源/追踪）**：`CommandMeta.timeout_ms` → gateway 转 wire
  `deadline_ns`（沿 ClearEStop）→ S0 action 用 `clock.now() > deadline` 判
  DeadlineExceeded。
- **D-P3（e2e heartbeat 源）= fake heartbeat 走真实 topic（非阻断已拍）**：fake
  heartbeat publisher 发到 **`/<robot_id>/s1/heartbeat`**（S0 订阅，`s0_daemon.rs:354`）
  驱动**真** S0 heartbeat 路径 → Ready→Enabled.Active，**不**直接调 S0 内部函数
  （比 s1_daemon 确定、专测 SetEnabled ack 点）。
- **D-P4（idempotency）= 锁定（PB3，非待拍）**：SetEnabled 是写 RPC（§6.7 #5）→
  **走 gateway `IdempotencyCache`**（`idempotency_key` fingerprint，沿 ClearEStop）；
  replay（同 key）→ 返 **cached `Ack`**（不重进 S0）。**⚠ replay 只在第一个 commit
  后成立（PB6）**：in-flight 窗口同 key 仍 `FreshInsert`→forward→S0 single-flight
  ResourceExhausted（详 D-P1 条 1）；**不**加 gateway in-flight guard。**Ack 语义**：
  SetEnabled 返
  **frozen 通用 `Ack`**（design §5.3），`cleared_at_ns` = **set-enabled 完成时刻**
  （Enabled.Active/Safe commit 的 daemon `clock.now()`；`dry_run=true` → "would-act
  at" eval 时刻；`status != Ok` → 0）。**名沿用 frozen Ack，语义文档化**（避免 e2e
  /summary 含糊把它当 ClearEStop 的 clear 时刻）。
- **D-P5（build_adapter 重构风险）**：`.first()`→`resolve_hardware_section` 改动
  adapter 构造路径（所有 config 走）；单 hardware（first==matched）0 变化，多
  hardware + CLI 选非 first 才变 → 回归须覆盖既有 smoke（crash_restart /
  scripted / ik 用单/多 hardware config）。

### P.4 Plan-Execute diff 登记（实施期填）

（占位：沿候选 A/B §P.4 体例。）
