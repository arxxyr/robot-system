# CodeMap · v0.5 候选 C Pre-Research — `SetEnabled` RPC

> RIPER Pre-Research（**只摸现状，不进 Spec/Plan/实现**）。覆盖 reviewer kickoff
> 的 5 个 focus：① design §5.3/§5.4 frozen 语义 + capability；② S0 enable/disable
> 状态机入口 + ClearEStop/ClearError control action 复用面；③ gateway → S0 RR
> 控制链路 envelope；④ enable/disable vs §6.7 #1/#3 边界；⑤ dimos auto_enable=True
> 最小行为。结论 = **additive**（proto method 8→9 + 新 RR envelope + S0 状态机
> 加 `Event::Disable`），开放问题留 Spec 拍板。

## amend 记录（2026-06-08；reviewer CodeMap review 后）

reviewer 4 阻断（事实错误）+ 1 非阻断收紧，已全回修：

| # | 原错误 | 实测纠正 |
|---|---|---|
| B1 | "set_enabled role 待拍/候选 C 新增" | `set_enabled` **已映射 operator/maintainer/admin**（`configs/rpc_roles.toml:54`，viewer 无）+ `Capability::SetEnabled` 已存在（`auth.rs:66`）→ 候选 C **复核锁定现有 operator+**，非新增 |
| B2 | "Event::Enable 当前无 production 触发器" | s0_daemon **启动时已自动发 `Event::Enable`**（`s0_daemon.rs:868`，尊重 estop_latched）+ ipc_thread drain 外部 event_rx（`ipc_thread.rs:554`）→ SetEnabled 是首个 **operator/RPC 显式** enable transport，**非首个 production enable** |
| B3 | "SCHEMA_SET_ENABLED = 0x0402/0x0403" | 0x0402/0x0403 已被 **InvokeTask** 占（`schema_id.rs:76`）；当前 max = `SCHEMA_IK_PAYLOAD=0x040C`（`:114`）→ SET_ENABLED 推 **0x040D/0x040E**（registry 升序 + schema_lint 锁）|
| B4 | "SetEnabled(false) 只加 Event::Disable" | enable **两层**：`Event::Enable` 只 Safe→Ready（`state_machine.rs:557`，无上扭）；真上扭是 `Ready+Heartbeat→Enabled.Active` 的 **effectful `AdapterNormalReq::Enable(true)`**（`:1124`）。disable 须拆清目标态 / OutputPolicy / 是否发 `Enable(false)` / 失败映射 / latched 时是否允许（hard_rt_ctrl：`Enable(true)` latched 拒、`Enable(false)` 仍可，`hard_rt_ctrl.rs:390`）|
| 非阻断 | §6.7 #3 disable 边界 | 补"disable 也不得清 latch，硬件侧必须是 torque-off/disabled path，**不是** EStop" |

---

## ① design §5.3/§5.4 frozen 语义 + capability

- **frozen RPC（design §5.3，`docs/00_design.md:938`）**：
  `rpc SetEnabled (SetEnabledCommand) returns (Ack);`
- **frozen message（design L908）**：
  `message SetEnabledCommand { CommandMeta meta = 1; bool enabled = 2; }`
  —— 单 bool `enabled`，无 per-hardware selector（**多 hardware 语义是开放问题**，
  类比候选 B 的 `GetState(Empty)` 歧义；见 §7 Q1）。
- **capability（design §5.4，L960）= 已落地 operator+（B1 纠正）**：
  `Capability::SetEnabled` **已存在**（`crates/robot-rpc/src/auth.rs:66`）；
  `configs/rpc_roles.toml:54` `[roles.operator].capabilities` **已含 `set_enabled`**
  （+ maintainer/admin 继承，**viewer 无**）。design §5.4 也无 maintainer+ 限定符
  （对比 `clear_estop (maintainer+)`）→ **operator+ 已是现状**。候选 C = **复核并
  锁现有映射**（确认 viewer 拒 + operator+ 准），**非新增/待拍**。
- **§5.5 路由**：`EStop` / `ClearEStop` 等 safety-critical control action **直接落
  S0 action handler，S1 不参与**；普通 task/policy/config 才转 S1。**SetEnabled 改
  S0 enable 状态（非 task）→ 归 S0 control action**（同 ClearEStop 侧，**非** S1）。
- **proto 现状**：`crates/robot-rpc/proto/robot_control.proto` **无** SetEnabled
  （仅在 L21"其它 §5.3 RPC 不在本 proto"注释列出）→ 候选 C = **additive method +
  message**（method 8→9，沿候选 B append 范式）。

---

## ② S0 enable/disable 状态机 + control action 复用面

### 状态机现状（`crates/robot-s0/src/state_machine.rs`）

- `S0Mode` = `Safe` / `Ready` / `Enabled{sub}` / `Fault(code)` / `Stopping`。
- **`Event::Enable` 已存在**（`Event` enum L122-133；transition L557）：
  `(Safe, E::Enable)` → `ready_guards_pass()` 满足则 `Ready`，否则 reject（留 Safe）。
- `(Ready, E::Heartbeat)` → `enabled_guards_pass()` 满足则 `Enabled.Active`（L580）
  —— **Enable 只把 Safe→Ready；Ready→Enabled 靠 heartbeat 自动**。
- `(Fault(_), E::ClearError)` → `!estop_latched && adapter_fault_cleared` 则 Ready
  （L635）。`(_, E::EStop)` → Safe（L542，压倒一切）。`(_, E::Stop)` → Stopping。
- **❗`Event::Disable` 不存在**（Event enum 无 Disable variant）。退出 Enabled 的
  现有路径只有：EStop→Safe / Stop→Stopping / AdapterErr→Fault / HeartbeatTimeout→
  CONTROL_LOST。**operator 主动 disable 无对应 transition** → 候选 C 需 **加
  `Event::Disable` + transition**（additive，目标态语义 §7 Q3）。

### `Event::Enable` 已有 production 触发器（启动 auto-enable 占位；**B2 纠正**）

- **s0_daemon 启动时已自动发一次 `Event::Enable`**（`s0_daemon.rs:868`，注释
  "自动 Event::Enable（Part 2 无 RPC，由 daemon 启动时替代 RPC 源）"）：配合
  all-true `initial_guards` 推 Safe→Ready，再由首个 heartbeat 推 Ready→Enabled。
  **尊重 estop_latched**（latched → `try_transition` 拒进 Ready，留 Safe）。
- `ipc_thread` 还 drain 外部 event channel（`event_rx.try_recv`，`ipc_thread.rs:554`）
  → `Event::Enable` 可被外部注入。
- s0_daemon 注释明写 "v0.3 起由 **RPC 网关**发；Part 2 简化：daemon 启动时立即发
  一次" —— **启动 auto-enable 是 RPC 源的临时占位**。
- **结论（纠正）**：`Event::Enable` **已有 production 触发器**（启动 auto-enable）；
  **SetEnabled 是首个 operator/RPC 显式 enable/disable transport**，把"daemon 无
  条件启动 auto-enable"升级成"operator 可控 + （§7 Q6）config `auto_enable` 绑定"。

### enable **两层**：状态机 event vs adapter 上扭（**B4 纠正**）

- **层 1（状态机）**：`Event::Enable` 只做 `Safe→Ready`（`state_machine.rs:557`，
  **Pure transition，无上扭**）。
- **层 2（硬件）**：真正上扭在 `Ready + Heartbeat → Enabled.Active` —— 这是
  **Effectful transition**，先排 `AdapterNormalReq::Enable(true)` 等 ack 才 commit
  Enabled（`state_machine.rs:1124` "finding M7：决定上扭后必须先排 adapter
  Enable(true) 等 ack，避免汇报 Enabled 但硬件未真上扭的窗口"）。
- **adapter latch 语义**（`hard_rt_ctrl.rs:390`）：`safety_latched` 时
  `Enable(true)` → `Unsupported`（拒）；`Enable(false)` → `Submitted`（仍可）。
- **对 SetEnabled 的含义**：`SetEnabled(true)` 走层 1 `Event::Enable`（latch 时层 1
  guard 已先拒）；`SetEnabled(false)`（新 `Event::Disable`）**不只是状态机 event**，
  必须明确**是否发 `AdapterNormalReq::Enable(false)`（torque-off）+ 目标态 +
  OutputPolicy + 失败映射 + latched 时是否允许 disable**（§7 Q3 收紧）。

### ClearEStop control action = 复用模板（`clear_estop_action.rs` + RR drain）

- A1 ClearEStop 链路：gateway `rr_io_thread` → S0 `rr_server_thread`
  （topic `/<robot_id>/control/clear_estop`，只做 wire I/O）→ crossbeam channel →
  `ipc_thread` RR drain（`ClearEStopRrRequest`，`ipc_thread.rs:103`）→ `ipc_tick`
  调 `clear_estop_action::try_apply_clear_estop` + 状态机 `E::ClearEStop` → 产
  **domain status** response（`ClearEStopRrPayload`）→ encode → send_response。
- **SetEnabled 可复用此 transport 骨架，但 action 更简单**：ClearEStop 有
  TorqueOffConfirmed gate + fsync latch + clear_safety_latch 三步硬顺序；**SetEnabled
  只是 guard-checked 状态机 event**（`E::Enable` / 新 `E::Disable`）+ adapter
  enable，**无 fsync / 无 latch / 无 TorqueOffConfirmed gate**。
- **ClearError 现状辨析**：design §6.7 状态机 `(Fault, E::ClearError)`→Ready 是
  **S0** 语义；但 B4 落地的 `ClearError` RPC 是 **S1 RR**（清 coordinator
  `Degraded`，`rr_clear_error` → S1）。**两套 ClearError 语义并存** → SetEnabled
  归 S0（同 ClearEStop），**不**走 B4 S1 task RR 路径（§7 Q4 确认）。

---

## ③ gateway → S0 RR 控制链路 envelope

- **gateway RR topics（`gateway.rs:1487 RrTopics::for_robot`）= 5**：
  `clear_estop`（→**S0**）+ `task_invoke`/`task_cancel`/`task_status`/`clear_error`
  （→**S1**）。**SetEnabled = 第 6 条 RR topic `/<robot_id>/control/set_enabled`
  → S0**（第 2 条 S0-bound，clear_estop 是第 1 条）。
- **S0 daemon rr_server（`s0_daemon.rs:360`）= 单 topic** `/<robot_id>/control/
  clear_estop`，单 `rr_server_thread`。SetEnabled 需 **第 2 个 S0 rr_server（
  set_enabled topic）或把单 rr_server_thread 泛化成多 topic** —— 实现选型 §7 Q5。
- **RR envelope（`crates/robot-msg/schemas/clear_estop.fbs`）**：`ClearEStopRequest`
  / `ClearEStopResponse` + `RpcRole` enum + `RpcStatus` enum（`RpcStatus`→
  `tonic::Code` 显式 match，newtype struct + count-lock 防漂移，**同候选 B PE1
  教训**）。**SetEnabled 需新 `set_enabled.fbs`**（`SetEnabledRequest{ meta 字段 +
  bool enabled }` + `SetEnabledResponse{ RpcStatus + domain status }`）+ schema_id
  注册（**B3 纠正**）：`0x0400`/`0x0401` = ClearEStop，`0x0402`/`0x0403` 已被
  **InvokeTask** 占（`schema_id.rs:76`），当前 **max = `SCHEMA_IK_PAYLOAD =
  0x040C`**（`schema_id.rs:114`）→ 新增 `SCHEMA_SET_ENABLED_REQUEST = 0x040D` /
  `_RESPONSE = 0x040E`（**registry 升序**，`SCHEMA_ID_NAMES` 表同步 + `schema_lint`
  锁），走 6 步 atomic 注册链（沿候选 1/2 schema 链先例）。
- **gateway handler**：reuse A1 `rr_io_thread` + `complete_ack_rpc` 桥范式（候选 A
  已熟）；SetEnabled 返 `Ack`（design §5.3）。

---

## ④ enable/disable vs §6.7 #1 / #3 边界（**安全硬约束**）

- **§6.7 #1（S0 存活 = 软件层安全充分条件）**：SetEnabled 走 S0 状态机 + adapter
  enable；S0 不在时靠硬件 fail-safe（command timeout）。SetEnabled **不改变**该
  保证（它只是 S0 内的 enable 状态转换，不绕过 S0）。
- **§6.7 #3（EStop 不可被软件清除为失忆态）—— 关键边界**：
  - `(Safe, E::Enable)` 的 `ready_guards_pass()` **包含 `!estop_latched`**
    （`state_machine.rs:189` + design §145）→ **`SetEnabled(true)` 在 estop_latched
    时被 guard 拒（留 Safe），无法 enable**。
  - **SetEnabled 全程不碰 `estop_latch` 文件**（只有 ClearEStop control action 能清
    latch，design §6.6.1 + §6.7 #3）→ §6.7 #3 **天然守住**。
  - `SetEnabled(false)`（新 `E::Disable`）**也必须不清 latch**：硬件侧必须走
    **torque-off / disabled path**（`AdapterNormalReq::Enable(false)`，latch 下仍
    `Submitted`，`hard_rt_ctrl.rs:390`），**不是** EStop（EStop 会置
    `estop_latched=true` + StopCommanded；disable 不置 latch）。
  - 不能从 Fault 直接 enable（Fault→Ready 只能 `ClearError`，`E::Enable` 从 Fault
    无 transition → 自然拒）。
- **hard boundary（候选 C 实施期不破）**：SetEnabled **不**清 estop_latch / **不**
  绕 FAULT / **不**改 ClearEStop·ClearError 语义 / **不**改 §6.7 任何不变量 /
  enable 必经 `ready_guards_pass`（含 !estop_latched）。

---

## ⑤ dimos auto_enable=True 最小行为

- **design §7.1 生命周期（L1196/1198）**：`[enable] write_enable(true)` /
  `[disable] write_enable(false) on shutdown` —— 这是 **adapter 硬件 enable**
  （`AdapterNormalReq::Enable(true/false)`，`rt_thread`/`hard_rt_ctrl` 已调），跟
  状态机 `Event::Enable`（Safe→Ready）是**两层**。
- **design §7.2 config（L1217）+ 现状**：`auto_enable = true` per `[[hardware]]`
  （`configs/xlerobot.toml:28` 已有）。**但 s0_daemon 启动 auto-enable 当前
  无条件**（`s0_daemon.rs:868` 不读 `auto_enable`，总发一次 `Event::Enable`）→
  **config `auto_enable` 字段目前未被 daemon 消费**（gap）。
- **候选 C 现状 vs 目标**：`Event::Enable` 已有启动 auto-enable 占位（B2）；候选 C
  提供 **operator/RPC 显式 enable/disable** 路径。**开放问题**（§7 Q6）= 候选 C
  是否**把现有无条件启动 auto-enable 绑定到 config `auto_enable`**（auto_enable=false
  的 hardware 启动不自动上扭，等 operator `SetEnabled(true)`），**非从零实现**。
  dimos-deploy 实际 auto_enable 行为开 Spec 前抽查 `/Users` 侧 dimos（[[dimos_paths]]
  移植参考源）确认最小动作 = enable-on-start。

---

## ⑥ 结论：implementation shape（additive）

候选 C = **additive**，沿候选 B（proto additive）+ A1（S0 control action 链路）+
候选 1/2（schema 注册链）三套已验证范式：

1. **proto**：append `rpc SetEnabled(SetEnabledCommand) returns (Ack)` + message
   `SetEnabledCommand{meta, bool enabled}`（method 8→9；既有 8 method 0 改）。
2. **robot-msg**：新 `set_enabled.fbs`（`SetEnabledRequest`/`SetEnabledResponse`）
   + schema_id 注册链 6 步 atomic（`SCHEMA_SET_ENABLED_*`）。
3. **gateway**：第 6 条 rr_io_thread（`/<robot_id>/control/set_enabled`→S0）+
   SetEnabled handler（auth/role/capability 校验 + `complete_ack_rpc` 桥）。
4. **S0**：第 2 个 rr_server（或泛化单 rr_server 多 topic）+ `ipc_thread` set_enabled
   RR drain → `ipc_tick` emit `Event::Enable`/新 `Event::Disable` → 状态机。
5. **状态机 + 硬件两层**：`Event::Enable` 复用现有（Safe→Ready，已 guard
   `!estop_latched`；上扭仍在 Ready→Enabled effectful 步）；**加 `Event::Disable`
   + transition**（目标态 / OutputPolicy / `AdapterNormalReq::Enable(false)` /
   失败映射 / latched-disable 由 §7 Q3 拍）。
6. **capability**：`set_enabled`→operator+ **已在** `configs/rpc_roles.toml:54` +
   `Capability::SetEnabled` 已存在 → 候选 C **复核锁定**（确认 viewer 拒），**非新增**。
7. **config（Q6）**：候选 C 是否把 s0_daemon 无条件启动 auto-enable 绑定到 config
   `auto_enable`（已有字段未被消费），待拍。

**不在候选 C**（推后/独立）：`EStop` RPC（候选 D）/ S1 任何改动（SetEnabled 是 S0
控制，S1 0 改）。

---

## ⑦ 开放问题（留 Spec 拍板，**本 CodeMap 不决**）

- **Q1（多 hardware 语义）**：`SetEnabledCommand` 只有 `bool enabled`，无 hardware
  selector。多 hardware 下 enable/disable **哪些**？（a）全 hardware；（b）加
  hardware_id（design-freeze diff，类比候选 B D3 `GetState(Empty)`→
  `GetStateRequest`，走 CHANGELOG）。倾向需 reviewer 拍。
- **~~Q2（role）~~ 已锁（B1）**：`set_enabled` **已是 operator+**
  （`rpc_roles.toml:54` operator/maintainer/admin，viewer 无 + `auth.rs:66`
  `Capability::SetEnabled`）→ 候选 C **复核确认现状**，非开放问题。
- **Q3（Disable 硬件侧语义收紧，B4）**：`SetEnabled(false)` = `Event::Disable` 须
  Spec 拍**五项**：(a) 目标态（→ Safe？复用 Safe vs 新 `Disabled` 子态）+ (b)
  `OutputPolicy`（Disabled / Hold？）+ (c) 是否发 `AdapterNormalReq::Enable(false)`
  torque-off + (d) adapter 失败如何映射 RpcStatus + (e) **latched 时是否允许
  disable**（hard_rt_ctrl 允许 `Enable(false)` latched）。共识：**不**置 estop_latched
  （区别 EStop）、**不**清 latch。
- **Q4（S0 vs S1 确认）**：SetEnabled 归 **S0 control action**（§5.5 + 改 S0 enable
  状态）→ 确认**不**走 B4 S1 task RR；S1 0 改。
- **Q5（S0 rr_server 选型）**：第 2 个 rr_server_thread（set_enabled 专用）vs 单
  rr_server_thread 泛化成多 topic（clear_estop + set_enabled）。
- **Q6（startup auto-enable 绑定 config，B2 重写）**：现状 = s0_daemon **无条件**
  启动 auto-enable（`s0_daemon.rs:868` 不读 config）+ config `auto_enable=true` 已
  存在但**未被消费**（`xlerobot.toml:28`）。候选 C 是否**把启动 auto-enable 绑定到
  config `auto_enable`**（false 的 hardware 启动不自动上扭，等 `SetEnabled(true)`）？
  还是只做 RPC 路径、config 绑定推后？**非从零实现 auto-enable**（已存在）。
- **Q7（dry_run）**：`CommandMeta.dry_run` 对 SetEnabled 语义（evaluate-only？）—
  ClearEStop 有 dry_run 先例，SetEnabled 是否支持端到端。

---

## ⑧ hard boundaries（候选 C 全程）

不跳 reviewer / 不动 §6.7（SetEnabled 不清 latch、不绕 FAULT、Enable 必经
`!estop_latched` guard）/ 不动 `docs/00_design.md`（SetEnabled 签名偏离走 CHANGELOG）
/ 不动 B4 frozen 8-method wire 既有字段 / 不动 ClearEStop·ClearError 语义 / A→B→C
→D→E 硬序（候选 D EStop / 候选 E dimos 不提前）。

> **下一步**：等 reviewer CodeMap review（5 focus + 7 开放问题方向）→ ACK 后进
> Spec round 1（**不**直接实现）。
