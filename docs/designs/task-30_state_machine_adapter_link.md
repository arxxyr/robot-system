# Task-30 设计方案：state_machine ↔ adapter 执行链路

> **状态**：**v3**，等再次 review
> **历史**：v1（commit `4cca734`，523 行草案）→ v2（commit `130b44b`，718 行）
> → v3（本版本，user v2 二轮 review 后修订）
> **作者**：Claude Code
>
> **关键诚实修正（v3 引入）**：v2 把 hardware IO 从 `s0_rt_thread` 移到 `adapter
> _io_thread`，但同时声称"不改设计"——这与 `docs/00_design.md:114`（"`s0_rt_
> thread` 调 HardRT adapter"，**冻结条款**）冲突。事实上 Feetech 串口 RTT ms 级
> **从来不是 HardRT**，但 `crates/robot-adapters/feetech/src/adapter.rs:202`
> 仍标 `RtClass::HardRT`（已在 v0.2 task 28 commit `940c563` 标注 "目标分类"
> 而非真值）。**v3 把这件事正式化**：新增 `task-30-pre-1` 把 Feetech 重分类
> 为 `SerialRT`，走 `docs/CHANGELOG.md` + `docs/10_verification.md` 评审，**不
> 假装是实现细节**。
>
> v3 不解决 v2 全部问题；v2 的 OutputPolicy / emergency 拆分 / OutputSafetyState
> 三大方向继续保留，但 v2 的具体实现里有 **ipc_thread 阻塞 / emergency 吞消息
> / SIGKILL 与 SLA 冲突 / 状态机不接受 SafetyStopFailed / state schema 缺字段
> / Velocity 假闭环** 6 个硬伤，v3 各章节修订。

---

## §0 v2 → v3 修订记录（user 二轮 review 落实）

User v2 review 提出 7 主问题 + 3 次问题，v3 全部接受 + 修订位置：

| # | v2 缺陷 | v3 修订位置 |
|---|---|---|
| 1 | **架构语义被改但声称"不改设计"**：v2 把 hardware IO 从 `s0_rt_thread` 移到普通优先级 `adapter_io_thread` 与 `00_design.md:114` 冻结条款冲突；Feetech 当前 `RtClass::HardRT` 标签是错的（串口 RTT ms 级根本不是 HardRT） | **§1.4 / §11 新增 `task-30-pre-1`**：Feetech 重分类 `SerialRT`；改 `feetech/src/adapter.rs:202` `rt_class` 字段；走 `docs/CHANGELOG.md` 设计修订评审 + `docs/10_verification.md` SerialRT 性能基线条目；**不混在 implementation detail 里** |
| 2 | `AdapterCtrlClient::enable()` 用 `bounded(1)` channel `send` 满时**阻塞** ipc_thread，`recv_timeout(2s)` 永远走不到；ipc_thread 卡死会停处理 heartbeat / watchdog / EStop | **§5.2 重写**：normal request 改 `try_send` + **pending op 异步语义**（state_machine 发请求即返回；下次 transition 前先 poll 上次 resp）；或 `send_timeout(50ms)` 紧上限；详 §5.2 二选一 |
| 3 | emergency `select!` 的 `recv(...)→ _ => {/* loop */}` 会**消费 emergency 消息但不处理**；SafetyStop 有 atomic 兜底但 `EStopLatch` 这类带语义的会丢 | **§5.4 重写** server loop：emergency `recv → handle_emergency(req)` **立即 dispatch**，不只 wakeup |
| 4 | `kill -SIGKILL → torque on（预期）` 与 `docs/10_verification.md:102` 冻结 SLA "kill -9 s0 → 硬件 fail-safe < 500 ms"（Step 10）冲突 | **§7 改写**：SIGKILL **不写"预期"**，列为 **blocking gap**；明确 task-30 不能闭环此项；推到 `task-30-post`：(a) Feetech servo 端 `Goal_Position` watchdog 标定（servo 固件层多久没新值就 hold/disable）；(b) 硬件继电器 / GPIO 急停（`docs/00_design.md` §0 / §6.7 范畴） |
| 5 | `safety_stop()` 失败后发 `Event::AdapterErr(SafetyStopFailed)`，但 `state_machine.rs:357` 只有 `Enabled.* → Fault`；EStop 已把状态变 `Safe`，再来 `AdapterErr` 从 `Safe` 状态会被拒绝 | **§6.3 新增**：`state_machine` transition 表加 **`* → Fault(FaultCode::SafetyStopFailed)`** 全状态接受（EStop / `Safe` / `Stopping` 也接收）；或与之等价：`OutputSafetyState` 升级为**与 state_machine 正交的独立权威状态**（见 §6.4 二选一 + 推荐方案） |
| 6 | state topic schema 不全：`crates/robot-msg/schemas/joint.fbs:36` 只有 `position_rad/velocity_rad_s/...`，**没有** `mode/control_link/output_mode`，更没 `safety_state`；`pub_thread.rs:237` 也没用 `state_rdr` 发状态机字段；冻结设计 §3.4 已要求 state topic 报 `mode/control_link/output_mode` | **§11 新增 `task-30-pre-2`**：补 `joint.fbs` 字段（`mode/control_link/output_mode/safety_state`）+ `build_helpers` + `pub_thread` 真发字段 + `robotctl status` 展示 + `schema_compat` 测试覆盖 |
| 7 | Velocity 假闭环：v2 `tick_hardware` 在 mode 变化时调 `set_mode`，**仍调 `write_command`**；但 `feetech/src/adapter.rs:455` 对非 Position 直接 no-op | **§5.6 改写**：`ActiveForward` 路径下 `cmd.mode != Position` → **要么** adapter 真支持（写 `Goal_Velocity` 寄存器，需 `set_mode` 切到 Velocity 后真闭环；v0.2 不实现）→ 要么 adapter `write_command` 显式返 `Err(RobotError::Unsupported)` → io_thread 接到 Err 上报 `Event::AdapterErr(Unsupported)` 进 Fault；**不再静默 no-op** |
| 次 1 | `RobotError::Unsupported` 不存在（`crates/robot-core/src/error.rs:28` enum 当前无此 variant），设计稿直接引用编译不过 | **§11 task-30-pre-3a**：`error.rs` 加 `Unsupported(&'static str)` variant + 同步 thiserror Display |
| 次 2 | 文档说 trait "现有 8 方法"，实际是 9（`crates/robot-core/src/adapter.rs:225`） | §1.1 / §5.7 勘误：trait 现有 9 方法（`info` / `connect` / `disconnect` / `read_state` / `write_command` / `set_mode` / `enable` / `safety_stop` / `limits`），加 `hold_current_position` 后 10 方法 |
| 次 3 | adapter_io_thread 用 `std::time::Instant::now()` 违反业务代码 RobotClock 规则（`CLAUDE.md` §业务代码禁用） | **§11 task-30-pre-3b**：adapter_io_thread 走 `RobotClock`（`MonoRawNs` / `Deadline`）；`Instant` 仅日志 `WallAndMono` 内允许 |

**v3 vs v2 行为变化要点**：
- `task-30` 不再是单一 task，**前置依赖** `task-30-pre-1/2/3` 必须先落地（实际是 3 个独立 small commit，10 min - 30 min 各）
- `task-30` 主体 M1-M5 拆分维持，但每个 M 内部修订（§5 全章节重写）
- **SIGKILL 不是 task-30 验收项**；SLA 的 fail-safe < 500 ms 升级为独立 blocking gap，进 `task-30-post`

v1 / v2 文档归档 git history（`4cca734` / `130b44b`）。v3 是当前权威。

---

## §1 问题陈述（v3）

### 1.1 双重缺口（v2 已识别）

#### A：state machine 不驱动 adapter

| 状态变化 | 期望 adapter 动作 | 当前 |
|---|---|---|
| `Safe → Ready` | `connect` + `set_mode(Position)` | ⚠️ spawn 时一次性，state_machine 不驱动 |
| `Ready → Enabled.Active` | 防飞 `enable(true)` | ⚠️ 同上 |
| `Enabled → Safe` | `enable(false)` 断扭 | ❌ |
| `Enabled → Fault` | `safety_stop()` | ❌ |
| `EStop` 事件 | `safety_stop()` 立刻断扭 | ❌ **安全级 bug** |
| `→ CtrlLostHold` | `hold_current_position()` 冻结 | ❌ |
| `cmd.mode = Stop` | `set_mode(Stop)` + `enable(false)` | ❌ adapter `return Ok` |
| `cmd.mode = Velocity` | `set_mode(Velocity)` + 真写 Goal_Velocity | ❌ adapter 仅 Position 静默 no-op |

#### B：`latest_cmd` 无门控

即便缺口 A 修了，io 层无条件转发 `LatestCommandSlot::latest_cmd` 会**冲掉 hold
/ Safe / Fault / EStop** 语义。v2 `OutputPolicy` 解决这一点（保留）。

### 1.2 v3 新发现：架构修订级问题

**Feetech 串口 RTT ≈ 13-motor sync_read 5 ms / sync_write 1 ms**（task 27c
flamegraph baseline）。这与 HardRT 50 Hz tick 的 < 100 µs 抖动目标差**两个
数量级**。`crates/robot-adapters/feetech/src/adapter.rs:202` 的注释原话：

> ```
> // adapter info 仍标 RtClass::HardRT，是按"目标分类"标的（task 28 上 IO
> // 线程 + atomic snapshot 后才是真 HardRT）
> ```

**这是一个延迟到现在仍未兑现的承诺**。v3 不再延期：

- 把 Feetech 实际类别**正式化为 `SerialRT`**（新增 RtClass variant，介于
  HardRT 和 SoftRT 之间：周期性 + 可预测但 RTT > 100 µs）
- 这是 `docs/00_design.md` §4.6 RtClass 定义的**扩展**，走设计修订评审

### 1.3 v3 新发现：state topic schema 与冻结设计不一致

`docs/00_design.md` §3.4 要求 state topic 报 `OutputMode` / `ControlLink` /
`Mode`，**冻结条款**。但当前实现：

- `crates/robot-msg/schemas/joint.fbs:36`：`JointState` 只有
  `position_rad/velocity_rad_s/torque_nm/timestamp_ns/layout_id`
- `crates/robot-s0/src/pub_thread.rs:237`：`pub_thread` 只读 `RtSnapshot`
  发出去，**没用** ipc_thread 写的 state_machine 字段（`OutputMode` 等）

这是另一笔已欠债。task-30 之前必须补完，否则 `safety_state` 字段加进来也没人
发。

### 1.4 v3 不动 §6.7 / §1.4 / §0 RT 铁律

但 §1.2 + §1.3 的修订**会**触及 §3.4（schema）+ §4.6（RtClass）+ §10.1（kill -9
SLA），这些**走 CHANGELOG 评审**作为设计修订条目。**`docs/00_design.md` 正文
不直接 edit**（依"冻结后文档卫生"机制，附录 E）。

### 1.5 优先级

- **EStop 不断扭** = 安全级 bug：与 `docs/10_verification.md:102` "kill -9 s0
  → 硬件 fail-safe < 500 ms"（Step 10 验收 SLA）相关
- **SIGKILL 部分**：纯软件无法保证；进 task-30-post（servo 标定 + 硬件层）
- 其他（Stop / Velocity / Hold 真闭环）= 功能缺失，阻塞 v0.3 + dimos 切换

---

## §2 设计原则（v3，含新增硬约束）

1. **§4.9 RT 铁律不变**：RT thread 不 alloc / 不 lock / 不 IO / 不 format
2. **§6.7 systemd 为最外层 supervisor 不变**
3. **`ManipulatorAdapter` trait 现有 9 方法签名不变**；加 `hold_current_position`
   默认 `Err(RobotError::Unsupported)` —— **trait 总数变 10**
4. **`panic = "abort"`**：robot-s0 不变
5. **现有单测 + task 29 真机闭环 100% 向后兼容**
6. **每个 commit 独立 CI 通过 + smoke**：增量落地
7. **安全链路不长期 feature-gated**：M1-M4 期间 off / M5 smoke 通过 on / 下个
   release 必须 on + 删 flag
8. **adapter 串口方法必须有有界 timeout**：禁止无限阻塞
9. **`safety_stop()` 失败 ≠ 普通 Fault**：升级 `OutputSafetyState::StopFailed`
   + `FaultCode::SafetyStopFailed`，**全状态接收** `* → Fault(SafetyStopFailed)`
10. **【v3 新增】不阻塞 state_machine 主循环**：`AdapterCtrlClient::*` 一律
    **非阻塞** 或 `≤ 50ms` 紧 timeout；不能因为 io 线程慢卡住 ipc_thread 处理
    heartbeat / watchdog / EStop 的能力
11. **【v3 新增】emergency 消息不丢**：select arm 立即 dispatch；不能只 wakeup
12. **【v3 新增】SIGKILL fail-safe 是 blocking gap**：不靠纯软件保证；硬件 +
    systemd + servo timeout 三层兜底，本 task 列为 known gap
13. **【v3 新增】schema 修订 → 改 fbs → 走 schema_compat 测试**：旧 reader 读新
    writer 必须能 fallback；新 reader 读旧 writer 必须不 panic

---

## §3 架构图（v3）

### 3.1 当前

（同 v2，略）

### 3.2 v3 后

```
┌─── robot-s0 ──────────────────────────────────────────────────────┐
│                                                                   │
│  ipc_thread (state_machine + iceoryx2 sub/pub 元数据)             │
│  ┌──────────────────────────────────────────────────────┐         │
│  │ try_transition (12 events) + transition hook          │         │
│  │   ↓                                                   │         │
│  │ AdapterCtrlClient（**非阻塞**接口）：                   │         │
│  │   - set_policy(p): atomic store (零等待)              │         │
│  │   - safety_stop(): try_send + AtomicBool 双兜底，     │         │
│  │     fire-and-forget，pending state 后台 poll          │         │
│  │   - request_normal(req): try_send → pending op slot， │         │
│  │     state_machine 在下次 tick 之前 poll 看 resp      │         │
│  │     超时 → 升级 AdapterErr → Fault                    │         │
│  └────────────────────────────┬─────────────────────────┘         │
│       │ atomic                │ try_send                          │
│       │ OutputPolicy          │ Emergency / Normal channels       │
│       ▼                       ▼                                   │
│  ┌─────────────────────────────────────────────────┐              │
│  │ adapter_io_thread (**SerialRT**, 普通优先级)     │              │
│  │ select! {                                       │              │
│  │   recv emergency_rx → handle_emergency(req)     │ ← 立即 dispatch│
│  │   recv normal_rx    → handle_normal(req, resp)  │              │
│  │   default(20ms)     → tick_hardware:            │              │
│  │     check estop_requested AtomicBool            │ ← atomic 兜底 │
│  │     match output_policy {                       │              │
│  │       Disabled / StopTorque → 不写 cmd          │              │
│  │       ActiveForward → write latest_cmd:         │              │
│  │         if cmd.mode != Position →               │              │
│  │           Err(Unsupported) → fault_tx          │ ← Velocity 修复│
│  │       HoldCurrent → hold_current_position()     │              │
│  │         Err → fallback safety_stop               │              │
│  │     }                                           │              │
│  │     read_state → HotShared.state_slot           │              │
│  │ }                                               │              │
│  │  + adapter (独占 SerialPort)                    │              │
│  └─────────────┬───────────────────────────────────┘              │
│                │ writes                                           │
│                ▼                                                  │
│         HotShared.state_slot (seqlock)                            │
│         HotShared.safety_state_slot (atomic)                      │
│                ▲                                                  │
│                │ reads                                            │
│  rt_thread (SCHED_FIFO，**纯数据通路**)                           │
│  ┌──────────────────────────────────────┐                         │
│  │ tick @ 50Hz：                        │                         │
│  │   read HotShared.cmd_slot            │                         │
│  │   forward to HotShared.state_slot'   │                         │
│  │   metrics (deadline / overrun)       │                         │
│  │   不碰硬件                            │                         │
│  └──────────────────────────────────────┘                         │
│                                                                   │
│  pub_thread（**v3 改**：reads state_slot' + state_machine 字段    │
│              → 发完整 JointState (position + mode + control_link  │
│              + output_mode + safety_state)）                      │
└───────────────────────────────────────────────────────────────────┘
```

主要变化：
- `adapter_io_thread` 显式标 **SerialRT**（不是普通 io_helper）
- `AdapterCtrlClient` 全接口非阻塞
- emergency `select` arm 立即 dispatch
- `cmd.mode` 非 Position → adapter 显式 `Err(Unsupported)` 进 Fault
- `pub_thread` 真发完整状态字段（schema 修订后）

---

## §4 OutputPolicy（v2 保留，v3 不变）

略（详 v2 §4，无修订）。映射表 + 7 variant + arc-swap 原语都保留。

---

## §5 详细设计（v3 修订）

### 5.1 `AdapterReq` / `AdapterResp` 拆分（v2 保留）

`AdapterEmergencyReq` / `AdapterNormalReq` 二分保留。详 v2 §5.1。

### 5.2 `AdapterCtrlClient`（**v3 重写：非阻塞**）

**v2 设计的硬伤**：`normal_tx.send(...)` 在 bounded(1) 满时阻塞 ipc_thread。

**v3 方案 A（推荐）**：**pending op slot** —— state_machine 发请求 = 写
slot；io_thread 读 slot 异步执行；state_machine 下次 transition 前先 poll 上
次 resp。

```rust
pub struct AdapterCtrlClient {
    /// state_machine 写 / io 线程读：当前要执行的 normal request（最多一条）
    pending_normal: Arc<arc_swap::ArcSwap<Option<AdapterNormalReq>>>,
    /// io 线程写 / state_machine 读：上一次 normal request 的结果
    last_normal_resp: Arc<arc_swap::ArcSwap<Option<AdapterResp>>>,
    /// emergency channel：try_send，不会阻塞
    emergency_tx: crossbeam_channel::Sender<AdapterEmergencyReq>,
    /// emergency atomic 兜底（channel 满时仍能传递 SafetyStop 意图）
    estop_requested: Arc<AtomicBool>,
    /// state_machine → adapter_io_thread 的 OutputPolicy
    policy_slot: Arc<arc_swap::ArcSwap<OutputPolicy>>,
}

impl AdapterCtrlClient {
    pub fn set_policy(&self, p: OutputPolicy) {
        self.policy_slot.store(Arc::new(p));
    }

    /// 紧急 - 同时设 atomic + 发 channel；fire-and-forget；不等 resp
    pub fn safety_stop(&self) {
        self.estop_requested.store(true, Ordering::Release);
        let _ = self.emergency_tx.try_send(AdapterEmergencyReq::SafetyStop);
    }

    /// 普通 - 写 pending slot；不阻塞 ipc_thread；返回前一次 resp 状态
    /// （让 state_machine 知道上次操作是否完成）
    pub fn request_normal(&self, req: AdapterNormalReq) -> NormalReqStatus {
        let prev_resp = self.last_normal_resp.swap(Arc::new(None));
        // 写新请求（覆盖旧的；旧的若 io 还没拿走会被丢，所以调用方需保证幂等）
        self.pending_normal.store(Arc::new(Some(req)));

        match (*prev_resp).clone() {
            Some(AdapterResp::Ok) => NormalReqStatus::PreviousOk,
            Some(AdapterResp::Err(e)) => NormalReqStatus::PreviousErr(e),
            None => NormalReqStatus::Submitted,
        }
    }

    /// state_machine 在下个 tick 前调用：拿到上次操作结果（不消费 pending）
    pub fn poll_last_normal_resp(&self) -> Option<AdapterResp> {
        (*self.last_normal_resp.load_full()).clone()
    }
}
```

**state_machine 用法**：

```rust
// transition hook
match (old, new) {
    (S0State::Safe, S0State::Ready) => {
        adapter_ctrl.request_normal(AdapterNormalReq::Connect);
        // 不等 —— 下次 transition tick 前 poll
    }
    _ => {}
}

// 每个 ipc_thread tick 头部
if let Some(AdapterResp::Err(e)) = adapter_ctrl.poll_last_normal_resp() {
    // 上次 connect/enable/set_mode 失败 → 触发 Fault transition
    self.fire_event(Event::AdapterErr(FaultCode::AdapterErr));
}
```

**v3 方案 B（备选）**：`send_timeout(50ms)` 紧上限 —— ipc_thread 最多卡 50ms，
不会停心跳。简单但仍有 50ms 阻塞，不如方案 A 干净。

**采用方案 A** —— 完全异步，state_machine 永不阻塞。

### 5.3 `IoState`（v2 保留，加 `current_mode_supports`）

```rust
struct IoState {
    connected: bool,
    enabled: bool,
    current_mode: ControlMode,
    /// 当前 mode 下 adapter 是否真支持 write_command。Position 总 true；
    /// Velocity / Torque 取决于具体 adapter 能力。set_mode 成功后从 adapter
    /// 查询填这个字段（v0.3+ 加 `ManipulatorAdapter::supports_mode` 方法）。
    /// v0.2 范围内只 Position = true 简化判定。
    current_mode_supports_write: bool,
    safety_state: OutputSafetyState,
    last_seen_cmd_mode: ControlMode,
}
```

### 5.4 `adapter_io_thread` 主 loop（**v3 重写：emergency 立即 dispatch**）

```rust
fn run_adapter_io(mut adapter: Box<dyn ManipulatorAdapter + Send>, ctx: AdapterIoCtx) {
    let mut io_state = IoState::default();
    let tick_period = Duration::from_millis(20);
    let mut next_tick = ctx.clock.now() + DurationNs::from(tick_period);

    while !ctx.stop.load(Ordering::Acquire) {
        // 1. estop AtomicBool 兜底（最先检查，channel 满 / 卡死也不漏）
        if ctx.estop_requested.swap(false, Ordering::AcqRel) {
            handle_safety_stop(&mut adapter, &mut io_state, &ctx);
        }

        // 2. select 主分发：emergency / normal / tick_deadline
        let now = ctx.clock.now();
        let until_tick = next_tick.0.saturating_sub(now.0);
        let timeout = Duration::from_nanos(until_tick.0);

        crossbeam_channel::select! {
            recv(ctx.emergency_rx) -> msg => {
                if let Ok(req) = msg {
                    // **立即 dispatch**（v3 修复 v2 的 wakeup-only 问题）
                    handle_emergency(&mut adapter, &mut io_state, &ctx, req);
                }
                continue; // 处理完一条 emergency 立即下轮，不进 tick
            }
            default(timeout) => {
                // tick 到期
            }
        }

        // 3. 处理一条 pending normal request（如有）
        let pending = ctx.pending_normal.swap(Arc::new(None));
        if let Some(req) = (*pending).clone() {
            let result = handle_normal(&mut adapter, &mut io_state, req);
            ctx.last_normal_resp.store(Arc::new(Some(match result {
                Ok(()) => AdapterResp::Ok,
                Err(e) => AdapterResp::Err(e),
            })));
        }

        // 4. tick_hardware（按 OutputPolicy）
        let policy = ctx.policy_slot.load_full();
        if io_state.connected {
            tick_hardware(&mut adapter, &mut io_state, *policy, &ctx);
        }

        // 5. 推进 next_tick；防漂移
        next_tick = next_tick + DurationNs::from(tick_period);
        let now2 = ctx.clock.now();
        if next_tick.0 < now2 {
            next_tick = now2 + DurationNs::from(tick_period);
        }
    }

    // shutdown 路径（不依赖 Drop）
    let _ = adapter.safety_stop();
    let _ = adapter.disconnect();
}

fn handle_emergency(
    adapter: &mut dyn ManipulatorAdapter,
    state: &mut IoState,
    ctx: &AdapterIoCtx,
    req: AdapterEmergencyReq,
) {
    match req {
        AdapterEmergencyReq::SafetyStop => {
            handle_safety_stop(adapter, state, ctx);
        }
        AdapterEmergencyReq::EStopLatch => {
            // **v3 修复**：不只 wakeup，真的写 latch 文件 + safety_stop
            write_estop_latch(&ctx);
            handle_safety_stop(adapter, state, ctx);
        }
    }
}
```

### 5.5 `handle_safety_stop` + `OutputSafetyState`（v2 保留 + v3 加 fault_tx）

```rust
fn handle_safety_stop(
    adapter: &mut dyn ManipulatorAdapter,
    state: &mut IoState,
    ctx: &AdapterIoCtx,
) {
    match adapter.safety_stop() {
        Ok(()) => {
            state.safety_state = OutputSafetyState::TorqueOffConfirmed;
            state.enabled = false;
        }
        Err(e) => {
            tracing::error!(?e, "safety_stop FAILED — hardware state UNKNOWN");
            state.safety_state = OutputSafetyState::StopFailed;
            // **v3**：发 SafetyStopFailed 到 ipc_thread；状态机 * → Fault 全状态接受
            let _ = ctx.fault_tx.try_send(FaultCode::SafetyStopFailed);
        }
    }
    ctx.hot_shared.safety_state_slot.store(state.safety_state);
}
```

### 5.6 `tick_hardware`（**v3 修：Velocity 真闭环或 Unsupported**）

```rust
fn tick_hardware(
    adapter: &mut dyn ManipulatorAdapter,
    state: &mut IoState,
    policy: OutputPolicy,
    ctx: &AdapterIoCtx,
) {
    // 先 read state（不论 policy）
    let mut buf = JointStateBuf::default();
    if let Err(e) = adapter.read_state(&mut buf) {
        tracing::warn!(?e, "read_state failed");
        return;
    }
    ctx.hot_shared.state_slot.publish(&buf);

    match policy {
        OutputPolicy::Disabled | OutputPolicy::StopTorque => {
            // 不写
        }
        OutputPolicy::ReadyHold => {
            // 已 connect 但 torque off；不需要主动写 Goal（torque off 时 Goal 不影响）
        }
        OutputPolicy::ActiveForward => {
            let cmd = ctx.hot_shared.cmd_slot.load();
            let view = cmd.as_view();

            // ControlMode::Stop → emergency safety_stop
            if view.mode == ControlMode::Stop {
                ctx.client.safety_stop();
                state.last_seen_cmd_mode = ControlMode::Stop;
                return;
            }

            // mode 变化 → set_mode 一次（不每 tick 调）
            if view.mode != state.current_mode {
                match adapter.set_mode(view.mode) {
                    Ok(()) => {
                        state.current_mode = view.mode;
                        // v0.2：仅 Position 真支持 write_command
                        state.current_mode_supports_write = view.mode == ControlMode::Position;
                    }
                    Err(e) => {
                        tracing::warn!(?e, mode = ?view.mode, "set_mode failed");
                        let _ = ctx.fault_tx.try_send(FaultCode::AdapterErr);
                        return;
                    }
                }
            }
            state.last_seen_cmd_mode = view.mode;

            // **v3 修复**：mode 不支持 → 上报 Unsupported 进 Fault，不静默 no-op
            if !state.current_mode_supports_write {
                tracing::error!(
                    mode = ?view.mode,
                    "adapter does not support write_command in this mode"
                );
                let _ = ctx.fault_tx.try_send(FaultCode::AdapterUnsupportedMode);
                return;
            }

            if let Err(e) = adapter.write_command(&view) {
                tracing::warn!(?e, "write_command failed");
            }
        }
        OutputPolicy::HoldCurrent => {
            match adapter.hold_current_position() {
                Ok(()) => {
                    state.safety_state = OutputSafetyState::TorqueOn;
                }
                Err(e) => {
                    // hold 不支持 → fallback safety_stop（"能 hold 就 hold；
                    // 不能 hold 就断扭；不能假装 hold 成功"）
                    tracing::error!(?e, "hold_current_position failed; fallback safety_stop");
                    ctx.client.safety_stop();
                }
            }
        }
        OutputPolicy::ZeroCommand => {
            // v0.2 退化为 hold；v0.3 加 Velocity 时改为写 Goal_Velocity=0
            if let Err(e) = adapter.hold_current_position() {
                ctx.client.safety_stop();
            }
        }
    }
}
```

### 5.7 `ManipulatorAdapter` trait 改动（v3 勘误）

trait 当前 **9 方法**（v2 误写 8）：`info` / `connect` / `disconnect` /
`read_state` / `write_command` / `set_mode` / `enable` / `safety_stop` /
`limits`。

v3 加 **第 10 方法**：

```rust
pub trait ManipulatorAdapter {
    // 9 现有方法 ...

    /// 冻结当前姿态。**默认 `Err(RobotError::Unsupported)`**。
    /// MockAdapter 显式 no-op；FeetechAdapter override 走 follow-current。
    fn hold_current_position(&mut self) -> Result<(), RobotError> {
        Err(RobotError::Unsupported("hold_current_position"))
    }
}
```

`RobotError::Unsupported(&'static str)` 见 §11 task-30-pre-3a。

---

## §6 OutputSafetyState + state_machine 修订（v3）

### 6.1 OutputSafetyState 枚举（v2 保留）

```rust
pub enum OutputSafetyState {
    TorqueOffConfirmed,
    TorqueOn,
    TorqueUnknown,
    StopFailed,
}
```

### 6.2 新 FaultCode（v3 加 `AdapterUnsupportedMode`）

```rust
pub enum FaultCode {
    AdapterErr,
    CmdDeadlineExpired,
    LimitViolation,
    WatchdogTimeout,
    RtOverrun,
    // v2 加
    SafetyStopFailed,
    AdapterUncertain,
    // v3 加（Velocity 修复）
    AdapterUnsupportedMode,
}
```

### 6.3 state_machine transition 表加 `* → Fault(SafetyStopFailed)`（v3 修复 review #5）

`crates/robot-s0/src/state_machine.rs:357` 当前只 `Enabled.* → Fault`。**v3
修订**：加全状态接受 `SafetyStopFailed`：

```rust
pub fn try_transition(state: S0State, event: Event, guards: &Guards) -> Option<S0State> {
    // 全状态最高优先级：safety_stop 失败 → 进 Fault（不论当前态）
    if let Event::AdapterErr(FaultCode::SafetyStopFailed) = event {
        return Some(S0State::Fault(FaultCode::SafetyStopFailed));
    }
    // ... 其他转换规则
}
```

> 这一改动**触及冻结条款 §1.4**（state_machine 转换表是 "冻结的转换表"）。
> 必须走 `docs/CHANGELOG.md` 设计修订评审 + 同步 dimos 上游 §1.4 转换表。
> 推荐：作为 task-30-pre-2 的一部分（state schema 修订都需要走评审）。

### 6.4 `OutputSafetyState` 是否独立权威？（v3 二选一）

**选项 A**：与 `S0State` 正交独立，对外 state topic 同时暴露两个字段（`output_
mode = state_machine 意图`，`safety_state = io 线程实测硬件`）。
- **优**：诚实，UI/S1 能区分意图 vs 实际
- **缺**：消费方需要理解两个字段语义（增加复杂度）

**选项 B**：合并到 `S0State::Fault(SafetyStopFailed)`，只一个字段。
- **优**：简单
- **缺**：丢失"我想停但没停成"这层细节

**v3 推荐 A**（与 user review #5 "明确 OutputSafetyState 是独立权威状态" 一
致）。schema 字段按 §11 task-30-pre-2 一并补。

---

## §7 SIGTERM graceful（v2 保留）+ SIGKILL **blocking gap**（v3 修订）

### 7.1 SIGTERM graceful（保留）

详 v2 §7。main shutdown controller 发 emergency safety_stop → bounded
timeout → disconnect → join → exit。不依赖 Drop。

### 7.2 SIGKILL：**不是 task-30 验收项，是 blocking gap**

`docs/10_verification.md:102` 冻结 SLA：

> kill -9 s0 → 硬件 fail-safe（torque disable/zero/brake） < 500 ms

**纯软件无法保证**。SIGKILL → 进程瞬间消失，无 signal handler，无 Drop，无
emergency channel send。任何"软件层 fail-safe < 500 ms"必须靠**硬件 / 固件
层**：

1. **Feetech servo 端 timeout**：`Goal_Position` 寄存器多久没新值就 hold 当前
   / disable torque（**v3 待标定**：`feetech_poke read 寄存器 X` + 物理实测找
   阈值）
2. **硬件继电器 / GPIO 急停**（设计 §0 / §6.7 范畴；v0.4+）
3. **systemd `TimeoutStopSec` + `KillMode=mixed`**：进程组干净停止（这条对
   SIGTERM 有效，对 SIGKILL 无效）

**v3 决议**：
- SIGKILL fail-safe < 500 ms **不是 task-30 验收项**
- 进入 **task-30-post**（独立 task），细化为：
  - **task-30-post-1**：Feetech servo `Goal_Position` watchdog 寄存器调研 +
    标定（servo 固件参数 / 寄存器名待 datasheet 确认）
  - **task-30-post-2**：硬件继电器选型 + 集成（v0.4+）
- `docs/10_verification.md:102` 该行**保留**（仍是冻结 SLA），但**移到
  blocking-gap 状态**直到 task-30-post 之后

---

## §8 测试覆盖（v3 扩）

v2 已列 9+ 用例，v3 加：

| 用例 | 验证 |
|---|---|
| `AdapterCtrlClient::request_normal` 不阻塞（pending slot 满时 overwrite） | 非阻塞语义 |
| `state_machine` transition hook 调 client → ipc_thread tick 时间 < 1 ms | 不卡心跳 |
| `Event::AdapterErr(SafetyStopFailed)` 从 `Safe` 状态触发 → 进 `Fault(SafetyStopFailed)` | * → Fault 全状态接受 |
| `EStopLatch` emergency 触发 → latch 文件真写入 + safety_stop 真调用 | 不丢消息 |
| `cmd.mode = Velocity` 在 v0.2 Feetech → `AdapterUnsupportedMode` Fault | Velocity 闭环 |
| `JointState` schema 含 `mode` / `control_link` / `output_mode` / `safety_state` 字段 | schema 修订 |
| `JointState` 旧 reader 读新 writer（缺新字段时 fallback 默认） | schema_compat |
| `pub_thread` 输出含 state_machine 字段（非空） | pub_thread 修订 |
| `robotctl status` 显示 `safety_state` | 用户可见 |
| **【blocking gap】** `kill -SIGKILL s0_daemon` → torque off **不通过**（标 known issue） | 不假装 |

---

## §9 迁移计划（v3：M1-M5 + 3 个 pre-task）

### 9.1 前置 task-30-pre

| # | 主题 | 工作量 | 依赖 / 评审 |
|---|---|---|---|
| **pre-1** | Feetech 重分类 `RtClass::SerialRT`（新增 enum variant + 改 feetech adapter `RtClass::HardRT → SerialRT` + 走 CHANGELOG 设计修订评审 + `10_verification.md` 加 SerialRT baseline 行）| 30 min + review | 设计修订级（§4.6 / §10.1 触及） |
| **pre-2** | state schema 补字段：`joint.fbs` 加 `mode/control_link/output_mode/safety_state` + `build_helpers` + `pub_thread` 真发字段 + `robotctl status` 展示 + `schema_compat` 测试 + `state_machine` transition 表加 `* → Fault(SafetyStopFailed)` | 1.5 h + review | 设计修订级（§3.4 / §1.4 触及） |
| **pre-3a** | `RobotError::Unsupported(&'static str)` variant + thiserror Display + `impl From<...>` 链路 | 15 min | 纯增量 |
| **pre-3b** | `adapter_io_thread` 走 `RobotClock`（替换 `Instant::now()`）；`MonoRawNs` / `Deadline` 已现成 | 30 min | 纯增量 |

**pre-1 + pre-2 必须先走 review + CHANGELOG 评审通过再合并**（设计修订级）。
**pre-3** 可直接合并（纯增量）。

### 9.2 主体 M1-M5（v2 保留 + v3 修订）

| # | 主题 | 工作量 | 依赖 |
|---|---|---|---|
| **M1** | `adapter_link` module skeleton（req / resp / client 非阻塞 / server / OutputPolicy / IoState 类型）+ mock 单测 | 1.5 h | pre-3a/b |
| **M2** | `OutputPolicy` + `tick_hardware` + 命令 gate 单测 | 1 h | M1 |
| **M3** | `hold_current_position` 默认 `Err(Unsupported)` + mock no-op + feetech override；trait 单测 | 30 min | pre-3a |
| **M4** | feature flag 启用：adapter_io_thread 接管 adapter；rt_thread 改读 HotShared；保留 legacy bootstrap；OutputSafetyState 写 state topic | 2 h | pre-1 / pre-2 / M1-M3 |
| **M5** | ipc_thread transition hook；删 legacy bootstrap；`ControlMode::Stop` 真断扭；`Velocity` Unsupported Fault；feature flag 默认 on；smoke 通过后下个 release 删 flag | 1.5 h | M4 |

**总 ~6.75 h**（v2 估 6 h 偏乐观；review 后真实化）。

**feature flag 时间线**：
- pre-1/2/3 / M1-M3 合并：flag 不存在
- M4：flag 加，**默认 off**
- M5：feature flag 默认 on
- **下一个 release 必须 on + 删 flag**（安全链路不长期 feature gated）

---

## §10 风险 + 回滚（v3 增）

### v2 R1-R5 保留。

### v3 新增

#### R6：pre-1 Feetech 重分类与上游 dimos 同步

`docs/00_design.md` §4.6 是从 dimos `docs/09_robot_framework.md` §4.6 复制而
来。重分类要回写 dimos 侧（"冻结后文档卫生"机制 / 附录 E）。短期独立改 robot-
system 侧 OK，长期回写。

#### R7：pre-2 schema 修订的兼容性

加新字段对旧 reader 默认安全（FlatBuffers 默认行为）。但**新 reader 读旧
writer**（缺新字段）必须 fallback 合理：
- `mode` 缺 → `ControlMode::Stop`（保守）
- `safety_state` 缺 → `TorqueUnknown`（不假装）
- `output_mode` / `control_link` 缺 → `Unknown`（schema 加 `Unknown` variant）

`schema_compat` 测试覆盖。

#### R8：pre-3 `Instant` → `RobotClock` 替换的隐性影响

`RobotClock::now()` 比 `Instant::now()` 略贵（多一次 syscall vs vDSO）。
adapter_io_thread 是非 RT，秒级周期，开销可忽略。但 `WallAndMono` 已用
`Instant`（日志层是基础设施例外）—— 保留不变。

---

## §11 v3 决议请求

请 review 并回复：

1. v3 整体（含 pre-1/2/3 前置 + M1-M5 主体 + SIGKILL 列 blocking gap）是否
   approve？
2. **pre-1 Feetech `RtClass::SerialRT`**：是否同意作为 `docs/00_design.md`
   §4.6 的设计修订条目（CHANGELOG 评审）？还是用现有 `SoftRT` 不新增 variant？
3. **pre-2 state schema 修订**：`mode` / `control_link` / `output_mode` /
   `safety_state` 全部加 `Unknown` variant 兼容旧 writer，对吗？
4. **§5.2 方案 A pending op slot 异步 vs 方案 B `send_timeout(50ms)` 紧上限**
   ：选 A 还是 B？
5. **`* → Fault(SafetyStopFailed)` 全状态接受**作为 §6.3 的修订是否需要
   走 `00_design.md` §1.4 转换表评审 + dimos 同步回写？
6. **§7.2 SIGKILL 列 blocking gap**：同意 task-30 不闭环此项，留 task-30-post
   做 servo timeout 标定 + 硬件继电器吗？
7. 真机最低门槛：M5 smoke 通过 = `feetech_poke read Torque_Enable=0`（mock_s1
   `--duration 0s` 正常退出后）+ 物理推夹爪 + `robotctl status` 看到
   `safety_state=TorqueOffConfirmed`。同意 / 加项？

review 通过我按 pre-1 → pre-2 → pre-3 → M1-M5 顺序动手 task-30 实施。
