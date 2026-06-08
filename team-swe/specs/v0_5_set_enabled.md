# Spec · v0.5 候选 C — `SetEnabled` RPC（round 2）

> RIPER Spec round 2（round 1 D2/D4/D5 ACK + 4 语义阻断回修）。前置 =
> [`team-swe/codemap/v0_5_set_enabled.md`](../codemap/v0_5_set_enabled.md)。
> **不开实现。**

## Spec round 2 amend 记录（2026-06-08；reviewer Spec round 1 复核后）

reviewer ACK D2/D4/D5；D1/D3/D6/D7 各 1 语义阻断，已回修：

| # | round 1 阻断 | 修订 |
|---|---|---|
| B1（D3）| "disable adapter 失败留原态" 与现有 effectful 机制冲突（`ipc_tick.rs:199` 失败 → `commit_fault(AdapterErr)` + `Disabled`）| **disable 失败 → Fault(AdapterErr) + `RpcStatus::AdapterError`/`Internal`，不返 Ack success**（沿现有 plan_transition/pending effectful 机制，**不**自定义 handler 扩范围）+ 补 AC 失败路径 |
| B2（D6）| auto_enable 绑定未按 resolved hardware_id 定义（hardware_id = CLI 或 `[[hardware]].first()`，`s0_daemon.rs:260`；build_adapter 取 `.first()` `:1061`；auto_enable 在 untyped `extras` `config_loader.rs:72`）| **D6 定义 resolution**：match `[[hardware]].id == resolved hardware_id` → 读该段 `auto_enable`（**加 typed `auto_enable: Option<bool>`**，缺省 true）；**无匹配 → 启动 fail-fast**（config/CLI mismatch）|
| B3（D1）| "whole-robot enable" 越过当前模型（单 RR service 做不到多 S0 fanout）| **D1 改"单 S0 daemon 实例 / selected hardware instance enable/disable"**；多 S0 / per-hardware / whole-robot fanout **推后** → `bool enabled` 不加 hardware_id 自洽 |
| B4（D7）| dry_run "would-succeed" 过度承诺（不执行 `Enable(false)` 无法知 adapter ack）| **dry_run 返 "would_accept / precondition result"**（状态机/guard/precondition 接受性，**明确不代表 adapter 会 ack**）；AC-6 锁 0 state / 0 adapter request / 0 latch + response 文案不误导 |

## What

实现 design §5.3 的 `SetEnabled` 控制 RPC —— operator 显式 **enable/disable
机器人**（S0 enable 状态机的首个 operator/RPC transport）：

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
  `bool enabled`）+ `SetEnabledResponse`（`RpcStatus` + domain status）。沿
  `clear_estop.fbs` 字段范式（`role` 仅 audit、S0 不授权）。
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
  `config_loader.rs` 加 **typed `auto_enable: Option<bool>`**（D6/B2）+ **s0_daemon
  启动 auto-enable 按 resolved hardware_id 绑定 + 无匹配 fail-fast（D6/B2）**。
- `docs/CHANGELOG.md`：登记（Q1 不加 hardware_id 故**不**触 design-freeze diff；
  SetEnabled write-RPC idempotency；auto_enable typed 字段 additive）。
- e2e：cross-process（enable/disable happy + **disable adapter 失败→Fault** +
  latch-blocked enable + latched-disable not-clear-latch + dry_run precondition +
  auth + auto_enable resolution）。

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
| (d) | adapter 失败映射（**B1 收紧**）| 沿**现有 effectful 机制**（`ipc_tick.rs:199`）：adapter `Enable(false)` ack 失败 → `commit_fault(FaultCode::AdapterErr)` + `OutputPolicy::Disabled` → S0 domain status → `RpcStatus::AdapterError`（/ `Internal`）→ tonic Code。**不返 Ack success**；**不**留原态、**不**自定义 handler（沿 plan_transition/pending，不扩范围）|
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
  2. startup 按 **resolved `hardware_id`** match `[[hardware]].id` → 读该段
     `auto_enable`。**缺省（None / 无字段）= true**（保留当前行为，既有 config 0
     行为变化）。
  3. **无匹配段**（CLI `--hardware-id` 不在 config）→ **启动 fail-fast**（exit，
     config/CLI mismatch；不静默 fallback）。
- `auto_enable=false` → 启动**不**自动 emit `Event::Enable`，留 Safe 等 operator
  `SetEnabled(true)`。让 SetEnabled 有意义（operator disable 后不被启动再 auto-enable）。
- **风险低**：default-true 保既有；唯一 behavior 变化是 `auto_enable=false` 段
  （当前无）。

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

### 其它（非拍板）

- **idempotency**（§6.7 #5 write RPC）：SetEnabled 走 gateway `IdempotencyCache`
  （`idempotency_key` fingerprint，沿 ClearEStop）；enable/disable 本身幂等，cache
  防重放副作用歧义。

## Acceptance Criteria（round 2）

- **AC-1**：proto additive —— append `SetEnabled` + `SetEnabledCommand`（method
  8→9）；既有 8 method diff=0；fbs 0 退化（`schema_lint` + `cargo test -p
  robot-msg`）；既有 RPC e2e（`task_rpc_e2e` / `state_rpc_e2e`）0 退化。
- **AC-2（enable happy）**：Safe + guards-ok → `SetEnabled(true)` → `Event::Enable`
  → Ready（heartbeat 后 Enabled.Active）；`Ack`。
- **AC-3（disable happy + D3）**：Enabled → `SetEnabled(false)` → `Event::Disable`
  → adapter `Enable(false)` ack → Safe（OutputPolicy Disabled）；`Ack`；**estop_latch
  文件 0 改**（disable 不清/不置 latch）。
- **AC-3b（disable adapter 失败，D3d / B1）**：Enabled → `SetEnabled(false)` →
  adapter `Enable(false)` **ack 失败** → `commit_fault(AdapterErr)` + `Disabled` →
  **返 `RpcStatus::AdapterError`（非 Ack success）**；state = Fault(AdapterErr)。
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
**Spec round 2（本文，全吸收）→ 待 reviewer round 2 复核** → Plan → 实施。
**本 round 不开实现。**
