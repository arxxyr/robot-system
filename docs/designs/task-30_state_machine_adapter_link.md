# Task-30 设计方案：state_machine ↔ adapter 执行链路

> **状态**：**v2，等再次 review**
> **作者**：Claude Code（v1：2026-04-25 草案；v2：2026-04-25 user review 后修订）
> **背景**：`docs/handoffs/2026-04-25_1.md` §7.5.2；task 29 session 暴露架构缺口
> **不改设计**：本文档仅**补充实现细节**，`docs/00_design.md` 冻结条款不动
>
> **核心问题（v1 review 重新定调）**：
>
> > v1 解决了"**谁拥有 adapter**"。
> > v2 必须同时解决"**什么时候允许 `latest_cmd` 写进硬件**"——后者才是安全闭环的核心。

---

## §0 v1 → v2 修订记录

User review 在 v1 上提出 12 条 critical 反馈，v2 全部接受 + 落实到具体章节：

| # | v1 缺陷 | v2 修订位置 |
|---|---|---|
| 1 | **缺 OutputPolicy 门控**（最大问题：`hold_current_position` 后 io_helper 仍然继续转发 stale `latest_cmd` → 破坏 hold 语义） | §4 新章节 OutputPolicy + 状态映射表；§5 server loop 严格按 policy 转发 |
| 2 | `ControlMode::Stop/Velocity` 缺口只是从 rt 挪到 io_helper，没有真闭环 | §5.4 io_helper 内维 `IoState { current_mode, ... }`；cmd.mode 变化触发 `set_mode`；`Stop` 走 emergency `safety_stop` |
| 3 | server loop 在断连 / 串口 Err 时会 CPU 100% busy spin | §5.4 改用 `crossbeam_channel::select!` + deadline 周期；不靠串口 RTT 当节拍 |
| 4 | `bounded(1)` 普通 + emergency 共用一个队列 → SafetyStop 可能被卡住 | §5.2 拆 emergency / normal 双 channel；§5.5 加独立 `emergency_stop_requested: AtomicBool` 让 io 线程每轮最先检查 |
| 5 | `hold_current_position` 默认 no-op 会**掩盖** "生产 adapter 没实现 follow-current" 的安全问题 | §5.7 trait 默认改 `Err(RobotError::Unsupported)`；mock 显式 no-op；生产 adapter 不支持 hold 时 state_machine **fallback 到 `safety_stop()`** |
| 6 | M3 合并后 io_helper 已接管 adapter，M4 才接 state_machine hook → M3 单独合并行为退化 | §6 拆 M1-M5；M4 保留 legacy bootstrap；M5 才删 |
| 7 | v1 §5.5 留 5.5.a "RT 直调 adapter" 退路 = 否定方案 F；表述上还说 "RT hot path 0 修改" | §3 / §5 删 5.5.a；明确 rt_thread 职责变成"数据通路 + tick 统计"，不碰硬件 IO；`io_helper` **改名 `adapter_io_thread`** |
| 8 | `safety_stop()` 失败简单升级 Fault → 软件状态 = Safe，硬件 = Torque on，**Safe 这个词掩盖了硬件失败** | §7 新章节 OutputSafetyState（TorqueOffConfirmed / TorqueOn / TorqueUnknown / StopFailed）+ 新 FaultCode（SafetyStopFailed / AdapterUncertain）；对外 state topic 加字段 |
| 9 | SIGTERM 测试只写"`safety_stop` 调用"，没说不能依赖 Drop / panic = abort 下 Drop 不跑 | §8 SIGTERM graceful 路径明确：main shutdown controller 发 emergency safety_stop → 等 bounded timeout → disconnect → join → exit；**不依赖 Drop** |
| 10 | 测试覆盖缺安全语义：stale cmd gate / mode change once / emergency preempt / busy spin / shutdown / timeout | §9 测试覆盖表加 9 个新用例 |
| 11 | feature flag 默认 / 走向不明 | §6 / §10 明确：M1-M4 默认 off → smoke 通过后 on → 发布必 on，**不长期保留两套路径**（安全链路不可永久 feature-gated） |
| 12 | 整个文档 framing 在"接 channel"，没在"输出门控" | §0 重写问题陈述；§4 OutputPolicy 提到顶层概念，先于通信机制 |

v1 文档归档到 git history（commit `4cca734`），不另存文件。v2 是当前权威。

---

## §1 问题陈述（v2 重述）

### 1.1 现状

`crates/robot-s0/src/state_machine.rs` 定义 5 主态 / 4 子态 / 12 事件 / Guards
/ 完整 transition 表 + 单测。运行期跑在 `ipc_thread`，写 iceoryx2 state topic
对外汇报状态。

### 1.2 双重缺口

#### 缺口 A：state machine 不驱动 adapter

| 状态变化 | 期望 adapter 动作 | 当前 |
|---|---|---|
| `Safe → Ready` | `connect()` + `set_mode(Position)` | ⚠️ 一次性 spawn 时做，state machine 不驱动 |
| `Ready → Enabled.Active` | 防飞 `enable(true)` 序列 | ⚠️ 同上 |
| `Enabled → Safe`（手动 / Stop） | `enable(false)` 断扭 | ❌ |
| `Enabled.Active → Fault` | `safety_stop()` | ❌ |
| `EStop` 事件 | `safety_stop()` 立刻断扭 | ❌ **安全级 bug** |
| `→ CtrlLostHold` 子态 | `hold_current_position()` 冻结 Goal | ❌（feetech 有 helper，没人调） |
| `cmd.mode = Stop` | `set_mode(Stop) → enable(false)` | ❌ adapter 直接 `return Ok` |
| `cmd.mode = Velocity` | `set_mode(Velocity)` 切模式 | ❌ adapter 仅 Position |

#### 缺口 B：`latest_cmd` 无门控

即便缺口 A 修了（state_machine 能调 adapter.safety_stop），如果 io 层每帧仍
**无条件**把 `LatestCommandSlot::latest_cmd` 写给硬件，就会出现：

```
t=0    心跳丢失
t=10ms state_machine: Active → CtrlLostHold
t=11ms hold_current_position()：读 Present=1500，写 Goal=1500
t=12ms io 层 tick：读 latest_cmd（旧 S1 命令 Goal=2500） → 写硬件 Goal=2500
t=13ms motor 追 Goal=2500，破坏 hold 语义
```

**Hold / Safe / Fault / EStop 都会被 stale command 冲掉**。这是 v1 没看到的核心。

### 1.3 双管齐下：所有权 + 输出门控

v2 必须同时回答：

1. **谁拥有 adapter**（v1 已答：`adapter_io_thread`）
2. **什么时候 io 线程才把 `latest_cmd` 写进硬件**（v2 答：`OutputPolicy`）

---

## §2 设计原则（硬约束，v1 + v2 合并）

1. **§4.9 RT 铁律**：RT thread 不 alloc / 不 lock / 不 IO / 不 format
2. **§6.7 systemd 为最外层 supervisor**：本 task 不做进程 / supervisor 改动
3. **`ManipulatorAdapter` trait 外部签名稳定**：现有 8 方法不动 + 加
   `hold_current_position`（默认 `Err(Unsupported)`）
4. **`panic = "abort"`**：robot-s0 仍 abort；`adapter_io_thread` 在同进程内
5. **现有单测 + task 29 真机闭环 100% 向后兼容**
6. **每个 commit 独立 CI 通过 + 真机 smoke**：增量落地
7. **安全链路不长期 feature-gated**：feature flag 仅 M1-M4 期临时；smoke 通过后默认 on / 发布必 on / 不留两套路径
8. **adapter 串口方法必须有有界 timeout**：禁止无限阻塞（io_helper 抢占不了同一个串口调用）
9. **失败 ≠ 静默升级 Fault**：`safety_stop()` 失败属于"硬件状态不明"，需要新 FaultCode + 对外 state topic 字段，不能让"Safe" 这个词掩盖

---

## §3 架构图

### 3.1 当前

```
┌─── robot-s0 ──────────────────────────────────────┐
│                                                   │
│  ipc_thread          rt_thread (SCHED_FIFO)       │
│  ┌─────────────┐     ┌────────────────────────┐   │
│  │state_machine│     │ adapter (move 进闭包)  │   │
│  └──────┬──────┘     │ ┌────────────────────┐ │   │
│         │            │ │FeetechAdapter+Bus  │ │   │
│         │ writes     │ │ + SerialPort       │ │   │
│         ▼            │ └────┬───────────────┘ │   │
│  iceoryx2 state ─────│      │ every tick:     │   │
│   (only reports)     │      ▼                 │   │
│                      │   write_command        │   │
│  adapter ←─ NOT HERE │   read_state           │   │
│                      └────────────────────────┘   │
└───────────────────────────────────────────────────┘
```

### 3.2 v2（task-30 落地后）

```
┌─── robot-s0 ──────────────────────────────────────────────────┐
│                                                               │
│  ipc_thread (state_machine)                                   │
│  ┌──────────────────────────────────────┐                     │
│  │ try_transition (12 events)           │                     │
│  │  ↓ on transition:                    │                     │
│  │  - 决定 OutputPolicy                  │                     │
│  │  - 发 AdapterReq via Client          │                     │
│  └────┬──────────────────┬──────────────┘                     │
│       │ AdapterReq       │ OutputPolicy (atomic / arc-swap)   │
│       │ via channel      │                                    │
│       ▼                  ▼                                    │
│  ┌─────────────────────────────────────────┐                  │
│  │ adapter_io_thread (普通优先级)           │                  │
│  │ ┌───────────────────────────────────┐   │                  │
│  │ │  IoState {                        │   │                  │
│  │ │    output_policy,                 │   │                  │
│  │ │    current_mode,                  │   │                  │
│  │ │    safety_state, ...              │   │                  │
│  │ │  }                                │   │                  │
│  │ └───────────────────────────────────┘   │                  │
│  │                                         │                  │
│  │ select! {                               │                  │
│  │   recv emergency_rx → safety_stop      │ ← 高优先          │
│  │   recv normal_rx    → SetMode/Enable...│                  │
│  │   default(20ms)     → tick:            │                  │
│  │     match output_policy {              │                  │
│  │       ActiveForward → write latest_cmd │                  │
│  │       HoldCurrent  → hold_current_*    │                  │
│  │       Disabled / StopTorque → 不写     │                  │
│  │     }                                  │                  │
│  │     read_state → HotShared.state_slot  │                  │
│  │ }                                      │                  │
│  │  + adapter (独占 SerialPort)           │                  │
│  └─────────────┬───────────────────────────┘                  │
│                │ writes                                       │
│                ▼                                              │
│         HotShared.state_slot (seqlock)                        │
│                ▲                                              │
│                │ reads                                        │
│  rt_thread (SCHED_FIFO)                                       │
│  ┌──────────────────────────────────────┐                     │
│  │ tick @ 50Hz：                        │                     │
│  │   read HotShared.cmd_slot            │                     │
│  │   write HotShared.state_slot' (转发)  │                     │
│  │   写 deadline / overrun metrics       │                     │
│  │   不碰硬件                            │                     │
│  └──────────────────────────────────────┘                     │
│                                                               │
│  pub_thread (从 state_slot' → iceoryx2 publish, 不变)         │
│  watchdog (不变)                                              │
└───────────────────────────────────────────────────────────────┘
```

**核心变化**：
- `adapter` 所有权：`adapter_io_thread` 独占（不再叫 io_helper —— 这个名字太轻，
  实际是硬件执行线程）
- 增加 **OutputPolicy** atomic（state_machine 写 / adapter_io 读，每 tick 检查）
- 增加 **emergency channel** 与 normal channel 分离 + atomic estop flag 兜底
- rt_thread 变成纯数据通路（读 cmd slot / 写 state' slot / 算 metrics），**不
  做硬件 RTT**

---

## §4 OutputPolicy（v2 新增核心章节）

### 4.1 定义

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputPolicy {
    /// `Safe` / `Fault` / `Stopping`：禁止转发 `latest_cmd`，要求 torque off
    Disabled,
    /// `Ready`：连接但 torque off，目标跟随当前位置（启动期 hold 防飞）
    ReadyHold,
    /// `Enabled.Active`：允许转发 `latest_cmd`（**唯一**写硬件 Goal 的 policy）
    ActiveForward,
    /// `Enabled.CtrlLostHold`：冻结当前姿态，**不**转发 `latest_cmd`
    HoldCurrent,
    /// `Enabled.CtrlLostZero`：写零速 / 零增量
    ZeroCommand,
    /// `Enabled.CtrlLostStop` / `EStop` / hard stop：断扭
    StopTorque,
}
```

### 4.2 状态 → policy 映射表

| `S0State` | `OutputPolicy` | adapter 应处于 |
|---|---|---|
| `Safe` | `Disabled` | torque off + 不连接（或已 disconnect） |
| `Ready` | `ReadyHold` | connected + Position mode + torque off |
| `Enabled { Active, … }` | `ActiveForward` | torque on + 转发 latest_cmd |
| `Enabled { CtrlLostHold, … }` | `HoldCurrent` | torque on + Goal=Present 冻结 |
| `Enabled { CtrlLostZero, … }` | `ZeroCommand` | torque on + 速度 0（位置模式下退化为 Hold） |
| `Enabled { CtrlLostStop, … }` | `StopTorque` | torque off |
| `Fault(…)` | `StopTorque` | torque off |
| `Stopping` | `StopTorque` | torque off + disconnect |

`OutputPolicy::from_state(S0State) -> OutputPolicy` 是**纯函数 + 常量表**，
state_machine 每次 transition 调一次写到 atomic。

### 4.3 实现：`Arc<arc_swap::ArcSwap<OutputPolicy>>`

`OutputPolicy` 16 字节（enum + repr），可走 `AtomicU8` 加映射；但选 `ArcSwap`
保持和现有 `LatestCommandSlot` 同 pattern，零额外原语。

`adapter_io_thread` 每个 tick 头部 `policy_slot.load()` 取最新 policy（开销
≈ 几 ns），分支处理。

---

## §5 详细设计

### 5.1 `AdapterReq` / `AdapterResp` 拆分（v2）

```rust
/// 紧急安全请求 —— 走独立 channel + AtomicBool 兜底，绝不被普通请求阻塞。
pub enum AdapterEmergencyReq {
    SafetyStop,            // disable torque
    EStopLatch,            // 写 EStop latch + safety_stop
}

/// 普通控制请求 —— 走 bounded(1) channel + oneshot 同步等结果。
pub enum AdapterNormalReq {
    Connect,
    Disconnect,
    Enable(bool),
    SetMode(ControlMode),
    HoldCurrentPosition,
}

pub enum AdapterResp {
    Ok,
    Err(RobotError),
}
```

### 5.2 `AdapterCtrlClient`

```rust
pub struct AdapterCtrlClient {
    emergency_tx: crossbeam_channel::Sender<AdapterEmergencyReq>,
    /// 与 emergency channel 互补的 atomic：万一 channel 也卡了，`adapter_io_thread`
    /// 每个 tick 头部检查 `estop_requested` 也能触发 safety_stop。
    estop_requested: Arc<AtomicBool>,
    normal_tx: crossbeam_channel::Sender<(AdapterNormalReq, oneshot::Sender<AdapterResp>)>,
    /// state_machine transition 后写这里，io 线程读这里。
    policy_slot: Arc<arc_swap::ArcSwap<OutputPolicy>>,
}

impl AdapterCtrlClient {
    pub fn set_policy(&self, p: OutputPolicy) {
        self.policy_slot.store(Arc::new(p));
    }

    /// 紧急 —— 同时发 channel + 设 atomic（双兜底）。不等 resp（fire-and-forget）。
    pub fn safety_stop(&self) {
        self.estop_requested.store(true, Ordering::Release);
        let _ = self.emergency_tx.try_send(AdapterEmergencyReq::SafetyStop);
    }

    /// 普通 —— 同步等 resp（2 s 超时）。
    pub fn enable(&self, on: bool) -> Result<(), RobotError> {
        let (tx, rx) = oneshot::channel();
        self.normal_tx
            .send((AdapterNormalReq::Enable(on), tx))
            .map_err(|_| RobotError::Internal("io thread queue closed"))?;
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(AdapterResp::Ok) => Ok(()),
            Ok(AdapterResp::Err(e)) => Err(e),
            Err(_) => Err(RobotError::DeadlineExceeded),
        }
    }
    // ... set_mode / connect / disconnect / hold_current_position 同模式
}
```

### 5.3 `IoState`（adapter_io_thread 内部）

```rust
struct IoState {
    connected: bool,
    enabled: bool,
    /// 上次成功 set_mode 的模式；用于检测 cmd.mode 变化是否需要重新调
    /// adapter.set_mode（而不是每 tick 都 set）。
    current_mode: ControlMode,
    /// adapter 真实硬件输出状态（v2 新增），写到 state topic 让 S1 / UI 看到诚实信息
    safety_state: OutputSafetyState,
    /// 最后一次 cmd.mode（用来检测变化）
    last_seen_cmd_mode: ControlMode,
}
```

### 5.4 `adapter_io_thread` 主 loop（v2 重写）

```rust
fn run_adapter_io(
    mut adapter: Box<dyn ManipulatorAdapter + Send>,
    ctx: AdapterIoCtx,
) {
    let mut io_state = IoState::default();
    let tick_period = Duration::from_millis(20); // 50 Hz baseline
    let mut next_tick = Instant::now() + tick_period;

    while !ctx.stop.load(Ordering::Acquire) {
        // ---- 1) 最高优先级：检查 atomic estop flag（兜底，channel 卡死也不漏）----
        if ctx.estop_requested.swap(false, Ordering::AcqRel) {
            handle_safety_stop(&mut adapter, &mut io_state, &ctx);
            // 不 continue —— 后续仍然处理周期性 tick（read state）
        }

        // ---- 2) drain emergency channel（多个 emergency 全 drain，不只一条）----
        while let Ok(req) = ctx.emergency_rx.try_recv() {
            match req {
                AdapterEmergencyReq::SafetyStop => {
                    handle_safety_stop(&mut adapter, &mut io_state, &ctx);
                }
                AdapterEmergencyReq::EStopLatch => {
                    write_estop_latch(&ctx);
                    handle_safety_stop(&mut adapter, &mut io_state, &ctx);
                }
            }
        }

        // ---- 3) 处理一条 normal request（如果有）----
        // 不在 select 里阻塞等，避免拖慢 hardware tick
        if let Ok((req, resp)) = ctx.normal_rx.try_recv() {
            let result = dispatch_normal(&mut adapter, &mut io_state, req);
            let _ = resp.send(match result {
                Ok(()) => AdapterResp::Ok,
                Err(e) => AdapterResp::Err(e),
            });
        }

        // ---- 4) 周期性硬件 tick：按当前 OutputPolicy 决定写什么 ----
        let policy = ctx.policy_slot.load_full();
        if io_state.connected {
            tick_hardware(&mut adapter, &mut io_state, *policy, &ctx);
        }

        // ---- 5) sleep until next tick ----
        let now = Instant::now();
        if now < next_tick {
            // crossbeam_channel::select! 兼容唤醒：emergency 来了立刻唤醒
            crossbeam_channel::select! {
                recv(ctx.emergency_rx_peek) -> _ => { /* 立刻 loop */ }
                default(next_tick - now) => { /* 睡到 deadline */ }
            }
        }
        next_tick += tick_period;
        // 防止 deadline 漂移（如果上一个 tick 超时，跳过中间 tick 而不是连续追）
        if next_tick < Instant::now() {
            next_tick = Instant::now() + tick_period;
        }
    }

    // ---- shutdown 路径（不依赖 Drop，§8 graceful）----
    let _ = adapter.safety_stop();
    let _ = adapter.disconnect();
}
```

**关键防 busy spin**：`select!` + `default(next_tick - now)` —— 没事干就睡到下个
deadline，emergency 来了立刻唤醒。即使 adapter `read_state` 失败 / 断连，主
loop 也按 20 ms tick 循环，**不会 CPU 100%**。

### 5.5 `tick_hardware`：按 policy 决定输出

```rust
fn tick_hardware(
    adapter: &mut dyn ManipulatorAdapter,
    state: &mut IoState,
    policy: OutputPolicy,
    ctx: &AdapterIoCtx,
) {
    // 先读 state，无论 policy 都更新（pub_thread 要发出去）
    let mut buf = JointStateBuf::default();
    if let Err(e) = adapter.read_state(&mut buf) {
        tracing::warn!(?e, "adapter.read_state failed");
        return;
    }
    ctx.hot_shared.state_slot.publish(&buf);

    // 根据 policy 决定是否写 Goal
    match policy {
        OutputPolicy::Disabled | OutputPolicy::StopTorque => {
            // 不转发 latest_cmd；torque 状态由 transition hook 通过 normal_rx
            // (Enable(false)) 已经设过；这里不重复发指令
        }

        OutputPolicy::ReadyHold => {
            // 已 connect 但 torque off；持续把 Goal 拉到 Present 防 enable 时跳
            // 仅当 cmd.mode 还没切到 Position 时才需要 set_mode
            // 不 enable 不 set_mode 也行 —— state_machine 进 Ready 时已通过
            // SetMode(Position) 切过
        }

        OutputPolicy::ActiveForward => {
            let cmd = ctx.hot_shared.cmd_slot.load();
            let view = cmd.as_view();

            // ControlMode::Stop 真 disable torque（不再吞默静默 return Ok）
            if view.mode == ControlMode::Stop {
                ctx.client.safety_stop(); // 走 emergency 路径
                state.last_seen_cmd_mode = ControlMode::Stop;
                return;
            }

            // mode 变化时调 set_mode（不每 tick 调）
            if view.mode != state.current_mode {
                if let Err(e) = adapter.set_mode(view.mode) {
                    tracing::warn!(?e, mode = ?view.mode, "set_mode failed");
                } else {
                    state.current_mode = view.mode;
                }
            }
            state.last_seen_cmd_mode = view.mode;

            if let Err(e) = adapter.write_command(&view) {
                tracing::warn!(?e, "write_command failed");
            }
        }

        OutputPolicy::HoldCurrent => {
            // 一次性 hold，下一帧不重复（hold_current_position 已经把 Goal=Present）
            // 但是为了防 motor 自己漂移，仍可以每 tick 重新调一次
            // 这里**不**转发 latest_cmd
            if let Err(e) = adapter.hold_current_position() {
                // 关键：hold 失败 → 升级到 safety_stop（§7 OutputSafetyState）
                tracing::error!(?e, "hold_current_position failed; falling back to safety_stop");
                ctx.client.safety_stop();
            }
        }

        OutputPolicy::ZeroCommand => {
            // v0.2 仅 Position 模式 → 退化为 hold（写 Goal=Present）
            if let Err(e) = adapter.hold_current_position() {
                ctx.client.safety_stop();
            }
            // v0.3 Velocity 真支持后这里写 Goal_Velocity=0
        }
    }
}
```

### 5.6 `handle_safety_stop` 与 OutputSafetyState

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
            // 上报到 state_machine：这不是普通 Fault，是 SafetyStopFailed
            let _ = ctx.fault_tx.send(FaultCode::SafetyStopFailed);
        }
    }
    // 把 safety_state 也广播到 HotShared，供 pub_thread 写到 state topic
    ctx.hot_shared.safety_state_slot.store(state.safety_state);
}
```

### 5.7 `ManipulatorAdapter` trait 改动

```rust
pub trait ManipulatorAdapter {
    // ... 8 个原方法不变 ...

    /// 冻结当前姿态：读当前 Present 写回 Goal。**不**动 Torque。
    ///
    /// **默认 `Err(RobotError::Unsupported)`**（v2 强制）—— 不是 no-op。
    /// 原因：`hold_current_position` 是**安全相关**动作；让默认实现 no-op 会
    /// 掩盖 "生产 adapter 没实现 follow-current" 的问题。`mock_*` 显式 override
    /// 为 no-op；生产 adapter 必须显式实现，否则 state_machine 会 fallback 到
    /// `safety_stop()`。
    fn hold_current_position(&mut self) -> Result<(), RobotError> {
        Err(RobotError::Unsupported("hold_current_position"))
    }
}
```

`MockAdapter` 显式：

```rust
impl ManipulatorAdapter for MockAdapter {
    // ...
    fn hold_current_position(&mut self) -> Result<(), RobotError> {
        Ok(()) // mock 没硬件，no-op 合理且显式
    }
}
```

`FeetechAdapter`：override 走 task 29 commit 4 已加的 sync_goal_to_present + retry。

state_machine fallback 逻辑（§5.5 已含）：`hold_current_position` 返回
`Unsupported` → io 线程升级为 `safety_stop()`，对外 state 字段 = `StopFailed`
→ "能 hold 就 hold；不能 hold 就断扭；**不能假装 hold 成功**"。

---

## §6 OutputSafetyState（v2 新增）

### 6.1 枚举

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputSafetyState {
    /// safety_stop / enable(false) 成功执行 + 验证（如读 Torque_Enable=0）
    TorqueOffConfirmed,
    /// 正常运行，torque on
    TorqueOn,
    /// 启动期 / 断连后未恢复，硬件状态未知
    TorqueUnknown,
    /// safety_stop 调用返回 Err —— 软件试图断扭但失败，**硬件可能仍 torque on**
    StopFailed,
}
```

### 6.2 新 FaultCode

`state_machine.rs::FaultCode` 加：

```rust
pub enum FaultCode {
    AdapterErr,
    CmdDeadlineExpired,
    LimitViolation,
    WatchdogTimeout,
    RtOverrun,
    // v2 新增
    SafetyStopFailed,    // safety_stop() 调用失败
    AdapterUncertain,    // io 线程报 read_state 多次连续失败 / 模式不一致
}
```

### 6.3 对外 state topic 加字段

`docs/00_design.md` §3.4 state schema 不动；本字段加在 ipc_thread 写
state topic 的额外字段（FlatBuffers schema 加 `safety_state: u8`）。设计
冻结条款 §3.4 是 `OutputMode`（Active / Hold / Zero / Stop），与本字段
**正交**：`OutputMode` = state_machine 的意图；`OutputSafetyState` = adapter
的实际硬件输出状态。两者并列让外部观察者能区分"我想 Safe vs 我成功 Safe"。

---

## §7 SIGTERM / 关停 graceful 路径（v2 新增）

`s0_daemon` main 收到 SIGTERM：

```text
1. signal handler 设 SHUTDOWN flag
2. main loop 检测 flag → graceful_stop()
3. graceful_stop():
   a. ipc_thread::request_stop()  → drain in-flight transitions
   b. AdapterCtrlClient::safety_stop()  → 走 emergency channel
   c. wait_response(timeout=1s) → 确认 OutputSafetyState=TorqueOffConfirmed
      或 StopFailed
   d. AdapterCtrlClient::disconnect()
   e. adapter_io_thread::request_stop() + join
   f. rt_thread::request_stop() + join
   g. pub_thread / watchdog::stop + join
   h. exit
4. 不依赖 Drop（panic = abort 时 Drop 不跑；正常退出也不该把安全动作放 Drop
   里 —— Drop 顺序 + panic 都不可靠）
```

`SIGKILL` / panic：进程被 OS 干掉。**软件层无法保证断扭**，必须靠：
- 硬件级：servo 内部 timeout（Feetech `Goal_Position` 多久没新值就 hold 当前）
- 进程层：systemd `TimeoutStopSec` 超时升级到 `KillMode=mixed` 让所有线程停
- **未来**：外部硬件急停 / 继电器（设计 §0 / §6.7 已写）

文档明确写：**这不是软件能 100% 保证的**，需要硬件 + 软件 + systemd 三层兜底。

---

## §8 测试覆盖（v2 扩）

| 类型 | 用例 | 验证 |
|---|---|---|
| Unit | `AdapterCtrlClient::safety_stop` 同时设 atomic + 发 channel | 双兜底 |
| Unit | `AdapterCtrlClient::enable` 超时 → `DeadlineExceeded` | timeout 处理 |
| Unit | `OutputPolicy::from_state` 全 8 状态 → 对应 policy | 映射表 |
| Unit | `IoState::current_mode` 持久 | mode change once 不重复 set |
| Unit | tick_hardware 在 `Disabled` policy 下不调 write_command | gate |
| Unit | tick_hardware 在 `HoldCurrent` policy 下不读 latest_cmd | gate |
| Unit | tick_hardware 在 `ActiveForward` 看到 `cmd.mode = Stop` → 触发 `safety_stop` | Stop 闭环 |
| Unit | tick_hardware 看到 `cmd.mode` 变化 → set_mode 调用 1 次 | mode change |
| Unit | `hold_current_position` Err → fallback `safety_stop` | trait fallback |
| Unit | `safety_stop` Err → `OutputSafetyState::StopFailed` + `FaultCode::SafetyStopFailed` 上报 | 诚实状态 |
| Integration | mock adapter：mock_s1 发 `Stop` → `robotctl status` 看到 `enabled=false` + `safety_state=TorqueOffConfirmed` | E2E |
| Integration | mock adapter：state_machine 进 `CtrlLostHold` → `latest_cmd` 不再被写硬件 | stale gate |
| Integration | mock adapter：`Safe`/`Fault` 状态下 S1 继续发 cmd → adapter 不被调 write_command | stale gate |
| Integration | emergency `SafetyStop` 发送时 normal channel 已满 → emergency 仍能立刻执行 | emergency preempt |
| Integration | adapter 断连 → adapter_io_thread CPU 占用 < 5%（不 busy spin） | 防 busy |
| Integration | SIGTERM s0_daemon → exit code 0 + 最后输出 OutputSafetyState=TorqueOffConfirmed | graceful |
| Integration | adapter 串口 `read_state` 卡死（mock 注入 sleep 5s）→ io 线程 select 超时 + 上报 `AdapterUncertain` | timeout |
| Soak | rt_guard_soak 扩：adapter_io_thread 连跑 60 s 不 leak / 不 alloc on hot path | regression |
| Orin 真机 | `feetech_poke read Torque_Enable` 在 S1 发 `Stop` 后应返 `0` | 真机 Stop |
| Orin 真机 | `kill -SIGTERM s0_daemon` → 物理摸 / 推夹爪 → torque off | graceful real |
| Orin 真机 | `kill -SIGKILL s0_daemon` → torque 仍 on（**预期**，需硬件层兜底） | 边界确认 |

---

## §9 迁移计划（v2：拆 M1-M5）

| # | 主题 | 工作量 | 行为变化 | 独立合并安全？ |
|---|---|---|---|---|
| **M1** | 新增 `adapter_link` module skeleton：req/resp/client/server + `OutputPolicy` + `IoState` 类型；mock 单测全套；**不接进 s0_daemon** | 1 h | 0 | ✅ 纯增量 |
| **M2** | `OutputPolicy` + `tick_hardware` + 命令 gate 单测落地（`Disabled` / `HoldCurrent` / `ActiveForward` 等场景）；adapter feature flag `state_machine_adapter_link = false` 默认 | 1 h | 0 | ✅ 仍未接 |
| **M3** | `ManipulatorAdapter::hold_current_position` 默认 `Err(Unsupported)`；mock 显式 no-op；feetech override 走 task 29 commit 4 路径；trait 单测扩 | 30 min | 0（fallback 让生产无感）| ✅ |
| **M4** | feature flag 启用：`adapter_io_thread` 接管 adapter 所有权；rt_thread 改读 `HotShared`；**保留 legacy bootstrap**（启动时 ipc_thread 仍发 `Connect` + `Enable(true)`，等价旧行为）；OutputSafetyState 字段写到 state topic | 2 h | 行为等价旧版本 | ✅（feature off 默认；on 时 legacy bootstrap 维稳） |
| **M5** | ipc_thread 加 transition hook，state_machine 真实驱动 adapter；删除 legacy bootstrap；feature flag 默认 on；`ControlMode::Stop` 真 disable torque + Velocity 模式真切换 | 1.5 h | **行为变** —— 安全增强（EStop 真断扭 / Stop 真断扭） | ⚠️ 这个 commit 是行为切换点；smoke 不过应 revert |

**合计 ~6 h**（v1 估 4.5 h 偏乐观；review 后真实化）。

**feature flag 时间线**：
- M1-M3 合并：flag 不存在
- M4：flag 加，**默认 off**
- M5 真机 smoke 通过：默认 on
- **下一个 release 必须 on + flag 删除**（不长期保留两套路径）

---

## §10 风险 + 回滚

### R1：`adapter_io_thread` 内死锁 / 阻塞
- 检测：`AdapterCtrlClient::*` 的 `recv_timeout(2s)` 超时 → state_machine 上报
  `Event::AdapterErr(SafetyStopFailed)` → 进 Fault
- 兜底：`estop_requested: AtomicBool` + `select!` deadline → 即便 channel 卡，
  下一 tick 也能跳出
- 回滚：feature flag off → 旧路径

### R2：rt_thread 一帧延迟（5.5.b）
- 量化：50 Hz 下 ≤ 20 ms。机器人控制余量 10-30 ms，可接受
- 缓解：rt_thread 与 io_thread 同步用 seqlock，无锁 + 无 alloc，延迟稳定

### R3：`ControlMode::Stop` 行为变化（语义增强）
- 当前 `feetech::set_mode(Stop)` 已调 `enable(false)`，行为一致化无副作用
- mock_s1 / robotctl 均没依赖旧行为

### R4：`hold_current_position` fallback 到 safety_stop 的语义变化
- 风险：如果某 adapter 漏实现 → 进 `CtrlLostHold` 子态时实际 disable torque
- 缓解：M3 加 trait 默认 + 单测要求 mock_*  / feetech 显式实现；CI 检查通过
- **预期行为**：fallback **更安全**（hold 不了 → 断扭比假装 hold 安全）

### R5：emergency channel 卡死
- 缓解：双兜底 atomic + channel；adapter 串口方法必须有 bounded timeout
- 不可解决场景：硬件层串口物理卡死（USB-RS485 卡） → 软件无能为力，需硬件
  watchdog / 外部继电器

### 回滚策略

每个 M commit 独立 revertable：
- M5 revert → ipc_thread 不接 hook，回到 legacy bootstrap
- M4 revert → adapter 重新 move 到 rt_thread，io_thread crate 保留无调用
- M3 revert → trait 默认从 `Err(Unsupported)` 回到 v1 的 no-op
- M2/M1 revert → 整个 module 删除

---

## §11 不做的事（留后续 task）

1. `robot-rpc` / `robot-viz` / `robot-policy-host` 的同类改造 → v0.4
2. rt_thread 降级后的 RT 必要性评估 → flamegraph 后回答
3. `ManipulatorAdapter` trait 大重构 → v0.4 真加 SoftRT adapter 时
4. B'c supervisor 联动 → 依赖本 task 完成后的 `Event::ServiceUnhealthy` 才能落
5. 硬件级急停（继电器 / GPIO）→ §6.7 / §10.1 范围，本 task 不动
6. servo 端 `Goal_Position` timeout fail-safe 的标定 → task-29 §4 待办

---

## §12 决议请求 v2

请 review 并回复：

1. v2 整体（含 OutputPolicy + emergency 拆分 + safety_state + M1-M5）是否 approve？
2. `OutputPolicy` 7 个 variant 是否覆盖完整？要不要加 `LimitedForward`（带 rate limit）等？
3. `OutputSafetyState` 与现有 `OutputMode` 并列写到 state topic，是否需要走
   设计冻结评审（`docs/00_design.md` §3.4 schema 字段加）？
4. M5 行为切换点（feature flag on）是否需要分两个 commit（"flag on" + "删
   legacy"）？
5. 真机验证最低门槛：M5 smoke 通过 = `feetech_poke read Torque_Enable=0` 在
   `mock_s1 --duration 0s` 后断扭 + 物理夹爪推动确认。同意 / 加项？

review 通过我按 M1-M5 顺序动手 task-30 实施。
