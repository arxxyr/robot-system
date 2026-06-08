# CodeMap · v0.5 候选 C Pre-Research — `SetEnabled` RPC

> RIPER Pre-Research（**只摸现状，不进 Spec/Plan/实现**）。覆盖 reviewer kickoff
> 的 5 个 focus：① design §5.3/§5.4 frozen 语义 + capability；② S0 enable/disable
> 状态机入口 + ClearEStop/ClearError control action 复用面；③ gateway → S0 RR
> 控制链路 envelope；④ enable/disable vs §6.7 #1/#3 边界；⑤ dimos auto_enable=True
> 最小行为。结论 = **additive**（proto method 8→9 + 新 RR envelope + S0 状态机
> 加 `Event::Disable`），开放问题留 Spec 拍板。

---

## ① design §5.3/§5.4 frozen 语义 + capability

- **frozen RPC（design §5.3，`docs/00_design.md:938`）**：
  `rpc SetEnabled (SetEnabledCommand) returns (Ack);`
- **frozen message（design L908）**：
  `message SetEnabledCommand { CommandMeta meta = 1; bool enabled = 2; }`
  —— 单 bool `enabled`，无 per-hardware selector（**多 hardware 语义是开放问题**，
  类比候选 B 的 `GetState(Empty)` 歧义；见 §7 Q1）。
- **capability（design §5.4，L960）**：`set_enabled` —— **无** `maintainer+` /
  `admin` / `any` 限定符（对比 `clear_estop (maintainer+)` / `estop (any)` /
  `reload_calibration (maintainer+)` / `set_policy (admin)`）。落在**默认 tier**
  （与 `read_state` / `invoke_task` / `clear_error` 同列）→ **倾向 operator+**，
  确切 role 由 `configs/rpc_roles.toml` 拍（候选 C 新增映射；§7 Q2）。
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

### `Event::Enable` 当前**无 production 触发器**

- `ipc_thread.rs` 模块 doc（L8-16）明写：运行期只真正发 `Enable`/`Heartbeat`/
  `EStop`，且 **`Enable` 来源 = "测试或 RPC 注入"**（表格 L13）。
- production `ipc_thread`（L535/548/552）只 push `Heartbeat` / `EStop` / 转发的
  `ev`；**无自动 push `Enable`**。`Event::Enable` 的全部 `events.push` 都在
  `ipc_tick.rs` **单测**里（748/976/1067…）+ `state_machine.rs` 单测。
- **结论**：今天 S0 production **没有 Safe→Ready 的 enable 触发器**；**SetEnabled
  是 `Event::Enable` 的首个 production transport**（与 ipc_thread doc"RPC 注入"
  的预留口对齐）。

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
  注册：现 `SCHEMA_CLEAR_ESTOP_REQUEST=0x0400` / `_RESPONSE=0x0401`（`schema_id.rs:
  66-69`）→ 新增 `SCHEMA_SET_ENABLED_REQUEST/RESPONSE`（如 `0x0402`/`0x0403`），
  走 6 步 atomic 注册链（沿候选 1/2 schema 链先例）。
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
  - `SetEnabled(false)`（新 `E::Disable`）**也必须不清 latch**（只 torque off）；
    且不能从 Fault 直接 enable（Fault→Ready 只能 `ClearError`，`E::Enable` 从 Fault
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
- **design §7.2 config（L1217）**：`auto_enable = true` per `[[hardware]]` ——
  dimos 语义 = 启动即 enable。robot-system 等价 = 启动时自动 emit `Event::Enable`
  （guards 满足时）**或** operator 显式 `SetEnabled(true)`。
- **候选 C 提供 operator/RPC enable 路径**；**config `auto_enable` 自动 enable
  是否本候选做**是开放问题（§7 Q6：倾向先做 RPC 路径，config auto_enable 可同候选
  或推后）。dimos-deploy 实际 auto_enable 行为需开 Spec 前抽查
  `/Users` 侧 dimos（[[dimos_paths]] 移植参考源）确认最小动作 = enable-on-start。

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
5. **状态机**：加 `Event::Disable` + transition（Enabled/Ready→Safe，**不清 latch**）
   ；`Event::Enable` 复用现有（已 guard `!estop_latched`）。
6. **capability**：`configs/rpc_roles.toml` 加 `set_enabled` → role（operator+ 倾向）。

**不在候选 C**（推后/独立）：`EStop` RPC（候选 D）/ config `auto_enable` 自动路径
（§7 Q6 待拍）/ S1 任何改动（SetEnabled 是 S0 控制，S1 0 改）。

---

## ⑦ 开放问题（留 Spec 拍板，**本 CodeMap 不决**）

- **Q1（多 hardware 语义）**：`SetEnabledCommand` 只有 `bool enabled`，无 hardware
  selector。多 hardware 下 enable/disable **哪些**？（a）全 hardware；（b）加
  hardware_id（design-freeze diff，类比候选 B D3 `GetState(Empty)`→
  `GetStateRequest`，走 CHANGELOG）。倾向需 reviewer 拍。
- **Q2（role）**：`set_enabled` capability → 哪个 role？design §5.4 非 maintainer+
  tier → 倾向 operator+；`configs/rpc_roles.toml` 落地。
- **Q3（Disable 目标态语义）**：`SetEnabled(false)` = `Event::Disable` →
  Enabled/Ready **回哪态**？倾向 → Safe（torque off，**不**置 estop_latched，区别于
  EStop）。是否需新 `Disabled` 子态 vs 复用 Safe，reviewer 拍。
- **Q4（S0 vs S1 确认）**：SetEnabled 归 **S0 control action**（§5.5 + 改 S0 enable
  状态）→ 确认**不**走 B4 S1 task RR；S1 0 改。
- **Q5（S0 rr_server 选型）**：第 2 个 rr_server_thread（set_enabled 专用）vs 单
  rr_server_thread 泛化成多 topic（clear_estop + set_enabled）。
- **Q6（config auto_enable 范围）**：候选 C 是否含 config `auto_enable` 自动 emit
  `Event::Enable`，还是只做 RPC 路径、auto_enable 推后？
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
