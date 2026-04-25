# Task-30 设计方案：state_machine ↔ adapter 执行链路

> **状态**：**v5**，等再次 review
> **历史**：v1（`4cca734`，523 行）→ v2（`130b44b`，718 行）→ v3（`f7bdc00`，
> 744 行）→ v4（`881e927`，458 行）→ v5（本版本，user v4 三轮 review 后修订）
> **作者**：Claude Code
>
> **v5 关键修订**：v4 三类 executor 并列方向对，但**安全动作的语义抽象缺失** ——
> state_machine 仍然要知道底层是 HardRT 还是 SerialRT 才能写正确的指令。v5 引
> 入 **`ExecutorCtrlClient` trait** 统一抽象：state_machine 只表达**意图**
> （`safety_stop` / `enable` / `set_policy`），实现层 executor 各自满足自己的
> RT 约束（HardRT 用 atomic action slot，SerialRT 用 emergency channel）。
>
> 同时修：HardRT 的 `StopTorque` 不能只是"不写命令"（与冻结设计 §0 §1.4 :149
> "cut output" 冲突）；`OutputPolicy` 改 `AtomicU8` 不用 `ArcSwap`（避免 RT
> hazard pointer）；Feetech 加 `safety_stop_fast()` 走 sync_write broadcast；
> ManipulatorAdapter trait 加 HardRT 安全动作合约。

---

## §0 v4 → v5 修订记录

User 在 v4 review 后给出 6 项 critical 修订：

| # | v4 缺陷 | v5 修订位置 |
|---|---|---|
| 1 | HardRT 安全动作只靠"不调 write_command"，不满足 §0 / §1.4 / `:149` "cut output / torque disable" 语义（"不写"≠"断扭"，motor 继续追旧 Goal） | **§4 新增 `ExecutorCtrlClient` trait**：HardRT impl 用 atomic action slot，rt_thread tick 内 RT-safe 执行 SafetyStop / Enable(false) / SetMode；SerialRT impl 用 emergency / normal channel；state_machine **只调 trait 方法**，不知道底层 |
| 2 | v4 §4 内部矛盾：标题"仅 SerialRT executor 使用"但下文又说 HardRT 也读 policy | **§3 重写**：`OutputPolicy` 是**所有 motor executor 的统一门控**（HardRT + SerialRT 都用）；执行动作因 executor 类型不同（HardRT 简化数据路径 gate + RT-safe action；SerialRT 完整语义 + emergency channel） |
| 3 | `ArcSwap<OutputPolicy>` 在 RT 路径有 hazard pointer 首次 alloc 风险（task 26 rt_guard_soak 已遇到）+ 语义复杂 | **§3.3 改 `AtomicU8` + `#[repr(u8)]`**：`OutputPolicy` 8 variant 单字节够用；RT 读是无锁、无 alloc、< 1ns；`u8 → enum` 转换走 `TryFrom` 或常量 match 表 |
| 4 | Feetech `enable(false)` per-motor 写 + 3× retry × 150ms = 最坏 5.85s 不适合 emergency 路径 | **§7 拆两方法**：`enable(false)` 保留可重试版本（普通路径）；**新增 `safety_stop_fast()`** 走 Feetech sync_write broadcast（id=0xFE 一次写所有 motor `Torque_Enable=0`，~1-2ms）；ack 不等；失败立刻 `OutputSafetyState::StopFailed` |
| 5 | SIGKILL 列 blocking gap 但 `docs/10_verification.md:102` 仍是 v0.2 SLA → 文档暗示 task-30 完成 = SLA 通过 | **§9 明确**：task-30 完成 **≠ v0.2 safety exit criteria 通过**；只完成"graceful 软件断扭"路径；硬件 fail-safe 留 task-30-post（servo timeout 标定 + 硬件继电器） |
| 6 | HardRT `ControlMode::Stop` 合约缺失（trait 没规定 `write_command(Stop)` 必须断扭，可以是 no-op） | **§5.3 ManipulatorAdapter trait doc 加合约**：HardRT adapter 必须实现 `write_command(mode=Stop)` 为 RT-safe cut output，或通过 `ExecutorCtrlClient` 路径统一处理；feetech 已实现（`adapter.rs:249-250` `Stop → enable(false)`）但慢，不能直接用 RT 路径 |

v1 / v2 / v3 / v4 文档归档 git history（commit ids 同上）。v5 是当前权威。

---

## §1 问题陈述（v4 保留）

### 1.1 三类 adapter，三条 executor

| `RtClass` | 典型硬件 | RT 合规性 | Executor |
|---|---|---|---|
| `HardRT` | EtherCAT / RT-CAN / DMA mmap / 自家 FPGA / shared-mem | 可证明 bounded latency + no alloc + no block | `s0_rt_thread` (SCHED_FIFO) |
| `SerialRT` | 串口舵机 / USB-RS485 / Modbus RTU | 控制语义属 S0，但 IO 不满足 RT 铁律（ms 级 RTT，syscall）| **新增** `s0_serial_io_thread`（普通 + RobotClock + OutputPolicy + emergency） |
| `SoftRT` | RealSense / webcam / vendor SDK / 网络 sensor | 仅产生观测，不直接驱动 motor | `s0_io_thread[0..N]`（v0.4 真用） |

判定基准：**可证明的最坏延迟 + RT 铁律合规性**，不靠协议名字。

### 1.2 双重缺口

#### A：state_machine 不驱动 adapter（EStop / Stop / Fault / CtrlLostHold / cmd.mode）

详 v3 §1.2.A。

#### B：`latest_cmd` 无门控（OutputPolicy 解决）

详 v3 §1.2.B + v5 §3。

### 1.3 Schema 已欠债（pre-2 修补）

`joint.fbs:36` / `pub_thread:237` / `state_machine` 转换表 `* → Fault(SafetyStop
Failed)`。详 v3 §1.3。

### 1.4 v0.2 safety exit criteria 状态

`docs/10_verification.md:102` "kill -9 s0 → 硬件 fail-safe < 500 ms" 是 v0.2
SLA。**task-30 完成不能宣称达成此 SLA**，只能完成"graceful 软件断扭" + "EStop
事件 → adapter.safety_stop 真实调用"。SIGKILL 后 motor torque off **必须靠硬件
+ servo 端 timeout** —— 留 task-30-post（独立 task）。

---

## §2 设计原则（v5 = v4 14 条 + v5 加 2 条）

1-14：v4 已定（详 v4 §2，全保留）。

**15（v5 新加）**：**state_machine 通过 `ExecutorCtrlClient` trait 表达意图**，
不知道底层 executor 类型。Executor 各自实现 RT-safe 执行。trait 抽象**强制
state_machine 不感知 RtClass 差异**，避免代码里出现 `if rt_class == HardRT
{ ... } else { ... }` 分支污染。

**16（v5 新加）**：**OutputPolicy 用 `AtomicU8`**（`#[repr(u8)]` enum + safe
`TryFrom<u8>`），不用 `ArcSwap` —— RT 路径无 hazard pointer / 无首次 load alloc /
< 1ns 读延迟。SerialRT 普通线程也用同一 atomic 保持类型一致。

---

## §3 架构图（v5 重画）

```
┌─── robot-s0 ────────────────────────────────────────────────────────────┐
│                                                                         │
│  ipc_thread                                                             │
│  ┌─────────────────────────────────────────────────┐                    │
│  │ state_machine                                    │                    │
│  │  ↓ on transition:                                │                    │
│  │  let policy = OutputPolicy::from_state(s0_state);│                    │
│  │  executor_ctrl.set_policy(policy);  ← 跨类型 atomic store             │
│  │  match transition_intent {                       │                    │
│  │    EnterSafe   => executor_ctrl.safety_stop()   │ ← fire-and-forget │
│  │    EnterReady  => executor_ctrl.request_normal( │                    │
│  │                     AdapterNormalReq::Connect)  │ ← async pending op │
│  │    EnterEnabled=> executor_ctrl.request_normal( │                    │
│  │                     AdapterNormalReq::Enable(true))                  │
│  │    EnterHold   => executor_ctrl.request_normal( │                    │
│  │                     AdapterNormalReq::HoldCurrent)                   │
│  │    ...                                          │                    │
│  │  }                                              │                    │
│  └─────────────┬───────────────────────────────────┘                    │
│                │ trait ExecutorCtrlClient                               │
│                ▼                                                        │
│  ╔═══════════════════════════════════════════════════════════════════╗  │
│  ║ Box<dyn ExecutorCtrlClient> (state_machine 持有；不知道实现类型)   ║  │
│  ╠═══════════════════════════════════════════════════════════════════╣  │
│  ║                                                                   ║  │
│  ║  s0_daemon::main 启动期 dispatch by adapter.info().rt_class:      ║  │
│  ║                                                                   ║  │
│  ║  HardRT executor:                  SerialRT executor:             ║  │
│  ║  ┌──────────────────────┐          ┌──────────────────────────┐   ║  │
│  ║  │HardRtCtrlClient impl │          │SerialIoCtrlClient impl   │   ║  │
│  ║  │ - policy: AtomicU8   │          │ - policy: AtomicU8       │   ║  │
│  ║  │ - action: AtomicU8   │ ←─ slot  │ - emergency_tx: ch       │   ║  │
│  ║  │ - pending_normal:    │   编码   │ - estop_atomic: AtomicBool│   ║  │
│  ║  │   ArcSwap (普通路径)  │          │ - normal_pending: ArcSwap │   ║  │
│  ║  │   仅冷路径，不入 RT  │          │ - last_resp:     ArcSwap  │   ║  │
│  ║  └──────────┬───────────┘          └────────────┬──────────────┘   ║  │
│  ║             │ shared via Arc                    │                  ║  │
│  ║             ▼                                   ▼                  ║  │
│  ║  ┌──────────────────────────┐      ┌────────────────────────────┐  ║  │
│  ║  │ rt_thread (SCHED_FIFO)   │      │ serial_io_thread (普通)    │  ║  │
│  ║  │ tick @ 50Hz：             │      │ select! {                   │  ║  │
│  ║  │   action = action.swap(0)│      │   recv emergency_rx → exec  │  ║  │
│  ║  │   if action != None:     │      │   recv pending normal       │  ║  │
│  ║  │     dispatch_action(...)  │      │   default(20ms) → tick:     │  ║  │
│  ║  │       (RT-safe SafetyStop│      │     check estop_atomic       │  ║  │
│  ║  │        / Enable(false)   │      │     check pending normal    │  ║  │
│  ║  │        / SetMode；       │      │     按 OutputPolicy gate    │  ║  │
│  ║  │        要求 adapter      │      │     mode 路由 / hold / Stop │  ║  │
│  ║  │        合约满足）         │      │     read_state → snapshot   │  ║  │
│  ║  │   policy = policy.load() │      │ }                           │  ║  │
│  ║  │   if ActiveForward:      │      │  - timeout 短到不破 EStop   │  ║  │
│  ║  │     write_command(cmd)   │      │    响应预算                 │  ║  │
│  ║  │   read_state             │      │  - safety_stop_fast() 路径  │  ║  │
│  ║  │   publish snapshot       │      │  独占 SerialRT adapter      │  ║  │
│  ║  │  独占 HardRT adapter     │      │  独占 SerialPort            │  ║  │
│  ║  └──────────────────────────┘      └────────────────────────────┘  ║  │
│  ║                                                                   ║  │
│  ╚═══════════════════════════════════════════════════════════════════╝  │
│                                                                         │
│  pub_thread / watchdog / health 不变                                   │
└─────────────────────────────────────────────────────────────────────────┘
```

**核心抽象**：

- `ExecutorCtrlClient` 是 **state_machine ↔ executor 的唯一接口**；state_machine
  不感知底层 RtClass 差异
- `OutputPolicy` 是 **跨 executor 类型的统一门控**（AtomicU8）
- HardRT 实现：action slot（atomic）让 rt_thread 在 tick 内 RT-safe 调 adapter
  RT-safe 安全方法（合约要求）
- SerialRT 实现：emergency channel + AtomicBool 兜底 + normal pending op；非 RT
  线程允许 syscall / blocking IO

---

## §4 ExecutorCtrlClient trait（v5 核心新增）

### 4.1 trait 定义

```rust
/// state_machine 的执行入口，跨 RtClass 统一抽象。
///
/// **不**暴露底层是 channel / atomic / mutex；调用方只表达意图。
pub trait ExecutorCtrlClient: Send + Sync {
    /// 设置 OutputPolicy（跨 RtClass 统一门控）。
    /// **RT 安全**（HardRT 实现走 AtomicU8 store；SerialRT 同）。
    fn set_policy(&self, policy: OutputPolicy);

    /// 紧急安全停。**fire-and-forget**；调用方**不等结果**。
    /// 实现层保证下次 tick / 短时间内执行；失败时 `OutputSafetyState::
    /// StopFailed` 通过 `safety_state_slot` 上报。
    fn safety_stop(&self);

    /// 普通控制请求（Connect / Disconnect / Enable / SetMode / HoldCurrent）。
    /// **不阻塞**；返回 `Submitted` / `PreviousOk` / `PreviousErr`。
    /// state_machine 在下次 transition 前 `poll_last_normal_resp` 检查上次结果。
    fn request_normal(&self, req: AdapterNormalReq) -> NormalReqStatus;

    /// 拉取上次 normal request 的执行结果（不消费）。
    fn poll_last_normal_resp(&self) -> Option<AdapterResp>;
}

#[derive(Clone, Copy, Debug)]
pub enum NormalReqStatus {
    Submitted,                      // 已写入 pending slot
    PreviousOk,                     // 上一条已成功
    PreviousErr(RobotError),        // 上一条失败
}

#[derive(Clone, Debug)]
pub enum AdapterNormalReq {
    Connect,
    Disconnect,
    Enable(bool),
    SetMode(ControlMode),
    HoldCurrentPosition,
}
```

### 4.2 HardRT 实现：`HardRtCtrlClient`

```rust
pub struct HardRtCtrlClient {
    /// `AtomicU8` 编码 OutputPolicy（< 1ns RT 读）
    policy: Arc<AtomicU8>,
    /// `AtomicU8` 编码下一 tick 要执行的 action：
    ///   0 = None
    ///   1 = SafetyStop
    ///   2 = Connect
    ///   3 = Disconnect
    ///   4 = Enable(true)
    ///   5 = Enable(false)
    ///   6 = SetMode(Position)  (RT-safe set_mode 仅 Position；其他 mode 走 normal)
    ///   7 = HoldCurrent
    action_slot: Arc<AtomicU8>,
    /// **冷路径** 普通请求（仅当 action_slot 编码不下时用，比如 SetMode(Velocity)）
    /// rt_thread 主循环**不**碰这个，只在 tick 间隙若 action_slot 为 None 才看 pending
    pending_normal: Arc<arc_swap::ArcSwap<Option<AdapterNormalReq>>>,
    last_normal_resp: Arc<arc_swap::ArcSwap<Option<AdapterResp>>>,
}

impl ExecutorCtrlClient for HardRtCtrlClient {
    fn set_policy(&self, policy: OutputPolicy) {
        self.policy.store(policy as u8, Ordering::Release);
    }

    fn safety_stop(&self) {
        // 写 action_slot；rt_thread 下一 tick 立即执行。AcqRel 保证 happens-
        // before 关系（state_machine 写 → rt_thread 读）。
        self.action_slot
            .store(ACTION_SAFETY_STOP, Ordering::AcqRel);
    }

    fn request_normal(&self, req: AdapterNormalReq) -> NormalReqStatus {
        // SafetyStop / Enable / SetMode(Position) 等可编码到 action_slot 的，
        // 改走 set_action 直接编码（fast path）；其他（如 SetMode(Velocity)
        // 带参数）走 pending_normal slot
        match action_encode(&req) {
            Some(a) => {
                self.action_slot.store(a, Ordering::AcqRel);
                NormalReqStatus::Submitted
            }
            None => {
                let prev = self.last_normal_resp.swap(Arc::new(None));
                self.pending_normal.store(Arc::new(Some(req)));
                map_status(prev.as_ref())
            }
        }
    }

    fn poll_last_normal_resp(&self) -> Option<AdapterResp> {
        (*self.last_normal_resp.load_full()).clone()
    }
}
```

**rt_thread 主循环消费 action_slot**：

```rust
fn run_rt_loop<A: ManipulatorAdapter + RtSafeCtrl>(
    cfg: RtThreadConfig,
    mut adapter: A,
    ctrl: Arc<HardRtCtrlInner>,
    ...
) {
    // ... pre-warmup ...

    loop {
        let deadline = ...;
        clock.sleep_until(deadline);

        rt_entry!({
            // 1. 检查 action_slot（tick 内 dispatch）
            let action = ctrl.action_slot.swap(ACTION_NONE, Ordering::AcqRel);
            if action != ACTION_NONE {
                // adapter 必须实现 RT-safe 版本；HardRT adapter 合约
                match action {
                    ACTION_SAFETY_STOP => {
                        let _ = adapter.safety_stop_rt();  // RT-safe trait method
                    }
                    ACTION_ENABLE_FALSE => {
                        let _ = adapter.enable_rt(false);
                    }
                    // ... 其他 action ...
                }
            }

            // 2. 检查 pending_normal（冷路径，仅当 action_slot 为空时）
            //    HardRT 多数 action 应在 action_slot 完成；pending_normal 只
            //    给"无法编码"的（SetMode 带参数等）。这部分实际上仍可能
            //    走 RT-unsafe 路径 —— **HardRT adapter 合约要求**这些方法也
            //    bounded（< RT tick 余量）。

            // 3. policy gate
            let policy_u8 = ctrl.policy.load(Ordering::Acquire);
            let policy = OutputPolicy::from_u8(policy_u8);
            if matches!(policy, OutputPolicy::ActiveForward) {
                let cmd_guard = cmd_reader.load();
                let cmd = cmd_guard.as_view();
                let _ = adapter.write_command(&cmd); // RT-safe per trait 合约
            }

            // 4. read_state + publish
            let _ = adapter.read_state(&mut state_buf);
            snapshot_pub.publish(...);
        });
    }
}
```

**关键约束**：HardRT adapter 必须实现 RT-safe 版本（见 §5）。如果 adapter 不
能保证 RT-safe `safety_stop`，**它就不该标 `RtClass::HardRT`**。

### 4.3 SerialRT 实现：`SerialIoCtrlClient`

```rust
pub struct SerialIoCtrlClient {
    policy: Arc<AtomicU8>,
    /// 紧急 channel（depth=8，emergency 不会被吞）
    emergency_tx: crossbeam_channel::Sender<AdapterEmergencyReq>,
    /// AtomicBool 兜底（channel 满时仍能传递 SafetyStop 意图）
    estop_requested: Arc<AtomicBool>,
    /// 普通 pending op
    pending_normal: Arc<arc_swap::ArcSwap<Option<AdapterNormalReq>>>,
    last_normal_resp: Arc<arc_swap::ArcSwap<Option<AdapterResp>>>,
}

impl ExecutorCtrlClient for SerialIoCtrlClient {
    fn set_policy(&self, p: OutputPolicy) {
        self.policy.store(p as u8, Ordering::Release);
    }
    fn safety_stop(&self) {
        self.estop_requested.store(true, Ordering::Release);
        let _ = self.emergency_tx.try_send(AdapterEmergencyReq::SafetyStop);
    }
    fn request_normal(&self, req: AdapterNormalReq) -> NormalReqStatus {
        let prev = self.last_normal_resp.swap(Arc::new(None));
        self.pending_normal.store(Arc::new(Some(req)));
        map_status(prev.as_ref())
    }
    fn poll_last_normal_resp(&self) -> Option<AdapterResp> {
        (*self.last_normal_resp.load_full()).clone()
    }
}
```

`serial_io_thread` 主循环详见 v3 §5.4 + v5 §7（含 `safety_stop_fast`）。

---

## §5 OutputPolicy 与 ManipulatorAdapter 合约（v5 重写）

### 5.1 OutputPolicy = 跨 executor 统一门控

```rust
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputPolicy {
    Disabled       = 0,  // Safe / Stopping / Fault：禁止转发 cmd；要求 torque off
    ReadyHold      = 1,  // Ready：connected 但 torque off；目标跟随 current
    ActiveForward  = 2,  // Enabled.Active：唯一允许写 latest_cmd
    HoldCurrent    = 3,  // CtrlLostHold：冻结当前姿态，不转发 cmd
    ZeroCommand    = 4,  // CtrlLostZero：写零速 / 零增量
    StopTorque     = 5,  // CtrlLostStop / EStop：断扭
}

impl OutputPolicy {
    #[must_use]
    pub const fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::ReadyHold,
            2 => Self::ActiveForward,
            3 => Self::HoldCurrent,
            4 => Self::ZeroCommand,
            5 => Self::StopTorque,
            _ => Self::Disabled, // 0 / 未知 → 保守
        }
    }
}
```

### 5.2 OutputPolicy 在两类 executor 的实现

| Policy | HardRT (rt_thread) | SerialRT (serial_io_thread) |
|---|---|---|
| `Disabled` | 不调 `write_command`；`adapter` torque 状态由 state_machine 通过 `Enable(false)` action 单独驱动 | 不调 `write_command`；torque 状态同左 |
| `ReadyHold` | 不调 `write_command`（adapter 已 connected + Position mode + torque off） | 同左 |
| `ActiveForward` | 调 `write_command(cmd)`；要求 RT-safe + bounded latency（HardRT adapter 合约）| 调 `write_command(cmd)`；含 `cmd.mode = Stop` → `executor_ctrl.safety_stop` 路径；`cmd.mode != Position` 在 v0.2 → `Err(Unsupported)` 进 Fault |
| `HoldCurrent` | 不调 `write_command`；hold 由 adapter 内部 / Enable(false) 已实现（HardRT adapter 多数支持 servo 内部 hold）| 调 `adapter.hold_current_position()`；失败 fallback `safety_stop` |
| `ZeroCommand` | 不调 `write_command`（v0.2 退化）| 同左 |
| `StopTorque` | 不调 `write_command`；torque 状态由 `Enable(false)` action 单独驱动 | 同左 |

**关键**：HardRT 路径"`Disabled` / `StopTorque`"的安全语义**不靠 OutputPolicy
本身**实现，**靠 action_slot（`SafetyStop` / `Enable(false)`）**单独触发。
state_machine 进 `Safe` 时同时调 `set_policy(Disabled)` + `safety_stop()`；
两者**并列且必要**。

### 5.3 ManipulatorAdapter trait 合约（v5 加 RT 子 trait）

trait 不变保持现有 9 + 1（hold_current_position）方法。

**新增子 trait `RtSafeCtrl`**：HardRT adapter **必须**额外实现，明确 RT-safe
版本：

```rust
/// 仅 `RtClass::HardRT` adapter 实现。每个方法**必须 RT-safe**：
///
/// - 不 alloc / 不 lock / 不 syscall（除 mmap reg write/read）
/// - bounded latency 可证明 < 100 µs（与 RT tick 余量匹配）
/// - 不 panic / 不 format
///
/// 不能保证以上的 adapter **不得标** `RtClass::HardRT`。
///
/// `ExecutorCtrlClient::safety_stop` / `enable(false)` 等 action 在 rt_thread
/// 主循环里调这些 RT-safe 方法。
pub trait RtSafeCtrl {
    /// RT-safe 断扭。EtherCAT：写 `Control_Word` 的 disable bit；DMA mmap：
    /// 写 torque enable bit = 0；不重试，不 sleep。
    fn safety_stop_rt(&mut self) -> Result<(), RobotError>;

    /// RT-safe enable / disable（不带防飞序列；防飞由调用方上游通过 `set_policy`
    /// + `read_state` follow-current 实现）。
    fn enable_rt(&mut self, on: bool) -> Result<(), RobotError>;

    /// RT-safe 模式切换（仅瞬时寄存器写）。
    fn set_mode_rt(&mut self, mode: ControlMode) -> Result<(), RobotError>;

    /// `write_command(mode = Stop)` 必须 **RT-safe cut output**：
    /// 至少等价 `enable_rt(false)`（即立刻断扭），**不**得 no-op / hold。
    /// 这是 HardRT adapter 的硬合约。
    ///
    /// 注：现有 `write_command(cmd)` 主签名不变；HardRT adapter 的实现里看到
    /// `cmd.mode = Stop` → 内部走 `enable_rt(false)`，不通过 ExecutorCtrl
    /// path（ExecutorCtrl path 是给 SerialRT 用的）。
}
```

**Mock + Feetech 不实现 `RtSafeCtrl`**：mock_hard_rt 是 RAM 内 simulation，
所有方法本来就 RT-safe（trivially），实施时可以直接 impl 一个 forwarding 版
本 + 单测。Feetech 是 SerialRT，不实现 `RtSafeCtrl`。

**未来 EtherCAT / RT-CAN adapter** 必须实现 `RtSafeCtrl`，否则不能标
`RtClass::HardRT`。

---

## §6 SerialRT executor 详细设计（v3 § 5 + v4 + v5 简化）

继承 v3 §5 全部内容（emergency / normal channel + IoState + select! server +
立即 dispatch + `tick_hardware` 按 policy gate）。

### 6.1 v5 修订 1：Atomic estop_requested 变化

v3 设计是 `AtomicBool estop_requested`。v5 仍保留作为 emergency 兜底，但
**一定走 `safety_stop_fast()` 路径**（见 §7），不再调 `enable(false)` 慢方法。

### 6.2 v5 修订 2：OutputPolicy 用 AtomicU8

详 §5.1。serial_io_thread 主循环每 tick 头部 `policy.load(Ordering::Acquire)`
+ `from_u8` 取 enum。

---

## §7 Feetech `safety_stop_fast()` 设计（v5 新增章节）

### 7.1 当前状态

`crates/robot-adapters/feetech/src/adapter.rs:336` `enable(false)`：

```
for name in &self.joint_names {
    for attempt in 1..=TORQUE_ENABLE_MAX_ATTEMPTS {  // 3
        match self.bus.write("Torque_Enable", name, ...) {
            Ok(()) => break,
            Err(_) => {
                sleep(TORQUE_ENABLE_RETRY_DELAY);  // 150ms
            }
        }
    }
}
```

13 motor × 3 retry × 150 ms = **最坏 5.85 s** —— **完全不能**走 emergency 路径。

### 7.2 新增 `safety_stop_fast()`

Feetech 协议支持 **sync_write 广播**（`motor id = 0xFE`）—— 一次写所有 motor
的同一寄存器。

```rust
impl FeetechAdapter {
    /// 紧急断扭：单次 sync_write 广播 `Torque_Enable=0` 给所有 joint。
    ///
    /// 时延：~1-2 ms（一次串口往返 + 不等 ACK + 不重试）。失败语义：上层调用
    /// 方应同时检查 `read_state` 的 Torque_Enable 字段确认；本方法**不保证**
    /// 硬件已断扭，只保证软件已下发。**这是 emergency 路径**，速度优先于
    /// 确认。
    ///
    /// 与 [`enable`] 的对比：
    /// - `enable(false)`：per-motor + 3× 重试 + 150ms = ~5.85s 最坏
    /// - `safety_stop_fast()`：1 次 sync_write broadcast = ~1-2ms
    ///
    /// **不替代** `enable(false)` —— 普通断扭仍走可重试版本；本方法仅 EStop /
    /// `Fault(SafetyStopFailed)` / `serial_io_thread::handle_safety_stop` 用。
    pub fn safety_stop_fast(&mut self) -> Result<(), FeetechError> {
        // sync_write Torque_Enable = 0 广播
        let entries: Vec<(u8, i32)> = self
            .joint_ids
            .iter()
            .map(|&id| (id, 0))
            .collect();
        self.bus.sync_write_by_id("Torque_Enable", &entries)?;
        // 不等 ACK，不重试
        Ok(())
    }
}
```

### 7.3 `serial_io_thread::handle_safety_stop` 走 fast 路径

```rust
fn handle_safety_stop(
    adapter: &mut dyn ManipulatorAdapter,
    state: &mut IoState,
    ctx: &SerialIoCtx,
) {
    // 优先用 fast path（如果 adapter 实现了）
    let result = adapter.safety_stop();  // trait method 内 feetech 实现走 fast
    match result {
        Ok(()) => {
            state.safety_state = OutputSafetyState::TorqueOffConfirmed;
            state.enabled = false;
        }
        Err(e) => {
            tracing::error!(?e, "safety_stop FAILED — hardware state UNKNOWN");
            state.safety_state = OutputSafetyState::StopFailed;
            let _ = ctx.fault_tx.try_send(FaultCode::SafetyStopFailed);
        }
    }
    ctx.hot_shared.safety_state_slot.store(state.safety_state as u8, Ordering::Release);
}
```

`ManipulatorAdapter::safety_stop` 默认实现走 `enable(false)`（保留向后兼容）；
`FeetechAdapter` override 走 `safety_stop_fast()`。这样 trait 不增加方法数（仍
9 + 1），只是 feetech 的 `safety_stop` 实现内部分流。

---

## §8 OutputSafetyState + state_machine 修订（v3 保留）

详 v3 §6（保留）+ pre-2 修订 transition 表加 `* → Fault(SafetyStopFailed)`。

---

## §9 SIGTERM graceful + SIGKILL **不闭环 v0.2 SLA**（v5 措辞调整）

### 9.1 SIGTERM graceful（v3 保留）

main shutdown controller 发 emergency safety_stop → bounded timeout →
disconnect → join → exit。不依赖 Drop。

### 9.2 SIGKILL：v5 重新措辞

**task-30 完成 ≠ v0.2 safety exit criteria 通过**。具体：

- task-30 **能完成**：
  - "graceful 软件断扭"路径（SIGTERM / `state_machine` 进 Fault → adapter 真断扭）
  - EStop 事件 → `adapter.safety_stop` 真实调用 + 持久化 latch
  - `feetech_poke read Torque_Enable=0`（mock_s1 退出后）+ `robotctl status
    safety_state=TorqueOffConfirmed`
- task-30 **不能完成**：
  - `docs/10_verification.md:102` "kill -9 s0 → 硬件 fail-safe < 500 ms"
  - 这是**硬件 + servo timeout** 才能保证的；纯软件做不到 SIGKILL 后的 fail-safe

**`docs/10_verification.md:102` 状态**应明确写：

```
| kill -9 s0 → 硬件 fail-safe (torque disable/zero/brake) | < 500 ms | — | — | — | ⏳ blocking gap (软件不可解；待 task-30-post servo timeout 标定 + 硬件继电器) | Step 10 / task-30-post |
```

`task-30-post`（独立 task，不在本 design scope）：
- post-1：Feetech servo `Goal_Position` watchdog 寄存器调研 + 标定（servo 端
  多久没新值 → hold / disable torque）
- post-2：硬件继电器 / GPIO 急停（`docs/00_design.md` §0 / §6.7 / v0.4+）

**v5 任何"task-30 完成 = v0.2 safety 通过"的暗示都要去掉**。

---

## §10 测试覆盖（v3 + v4 + v5）

继承 v3 / v4 全部 + v5 新增：

| 用例 | 验证 |
|---|---|
| `OutputPolicy::from_u8` 全 8 值 → enum 反向；超界 → `Disabled`（保守）| RT-safe atomic 反序列化 |
| `HardRtCtrlClient::safety_stop` → `action_slot = ACTION_SAFETY_STOP`；rt_thread tick 内 `swap` 取出并调 `adapter.safety_stop_rt()`（mock 验证）| RT 路径 |
| `HardRtCtrlClient::set_policy` 多次连续 → 最终值生效（atomic store/load 顺序）| RT 路径 |
| `HardRtCtrlClient::request_normal(SetMode(Velocity))` → 走 pending_normal slot（不入 action_slot）；rt_thread 在 tick 间隙调 adapter.set_mode（**HardRT adapter 合约要求**这条路径也 bounded）| 冷路径 |
| `SerialIoCtrlClient` v3 测试全保留 | 不回归 |
| `FeetechAdapter::safety_stop_fast` → 1 次 sync_write broadcast；txbytes 含 broadcast id `0xFE`；< 5ms 完成（mock 计时）| 紧急快路径 |
| `FeetechAdapter::safety_stop` 默认走 fast 不走 retry 慢路径 | feetech override |
| `serial_io_thread::handle_safety_stop` 调 `adapter.safety_stop`（feetech 内部走 fast）；失败上报 `OutputSafetyState::StopFailed` | 兜底 |
| `ManipulatorAdapter::write_command(mode=Stop)` 在 mock_hard_rt 触发 RT-safe cut output（不 no-op，写测试 enable_rt(false) 被调用）| trait 合约 |
| `ExecutorCtrlClient` trait 对象可被 state_machine 持 `Box<dyn>` 调用，不感知底层 | 抽象 |
| 同时跑 mock_hard_rt（HardRT， `HardRtCtrlClient`）+ feetech（SerialRT，`SerialIoCtrlClient`），state_machine 对两者用同一 trait API 调度 | 多 executor |
| **【blocking gap】** `kill -SIGKILL s0_daemon` → torque off **不通过**（标 known issue，留 task-30-post）| 不假装 |

---

## §11 迁移计划（v5 = v4 重排 + 加 ExecutorCtrlClient）

### 11.1 前置 task-30-pre（v3/v4 沿用）

| # | 主题 | 工作量 | 评审级别 |
|---|---|---|---|
| pre-1 | `RtClass::SerialRT` enum + feetech 重分类 + `00_design.md` §4.6 修订 + dimos 同步 + `10_verification.md` SerialRT baseline 行 | 30 min + 评审 | §4.6 设计修订 |
| pre-2 | state schema 补字段 + `pub_thread` 真发字段 + `robotctl` 展示 + `schema_compat` + state_machine 加 `* → Fault(SafetyStopFailed)` | 1.5 h + 评审 | §3.4 / §1.4 设计修订 |
| pre-3a | `RobotError::Unsupported(&'static str)` variant | 15 min | 纯增量 |
| pre-3b | `serial_io_thread` 走 RobotClock | 30 min | 纯增量 |

### 11.2 主体 M1-M6（v5 重排，新增 ExecutorCtrlClient + RtSafeCtrl + safety_stop_fast）

| # | 主题 | 工作量 | 依赖 |
|---|---|---|---|
| **M1** | 新增 `ExecutorCtrlClient` trait + `OutputPolicy(AtomicU8 + repr u8)` + `AdapterNormalReq` + `NormalReqStatus`；纯类型 + 单测 | 1 h | pre-3a/b |
| **M2** | 新增 `RtSafeCtrl` 子 trait（HardRT adapter 必实现）；`mock_hard_rt` 实现；`ManipulatorAdapter::write_command(mode=Stop)` 合约文档 + mock 测试验证 RT-safe cut output | 1 h | M1 |
| **M3** | `FeetechAdapter::safety_stop_fast()` + `safety_stop` override（默认走 fast）；单测 broadcast wire bytes + 快速时延 | 1 h | pre-1 |
| **M4** | `HardRtCtrlClient` 实现 + `rt_thread` 主循环消费 action_slot + policy gate；mock_hard_rt smoke 不回归 | 1.5 h | M1 / M2 |
| **M5** | `SerialIoCtrlClient` 实现 + `serial_io_thread` skeleton + `select!` loop + `tick_hardware`；feetech 走通；feature flag off | 2 h | M1 / M3 / pre-3b |
| **M6** | `s0_daemon` dispatch by `RtClass`；ipc_thread 加 `Box<dyn ExecutorCtrlClient>` + transition hook 真驱动；删 legacy bootstrap；feature flag default on；smoke 通过下个 release 删 flag | 1.5 h | M4 / M5 / pre-2 |

**总 ~8 h**（v4 估 6.75 h；v5 加 ExecutorCtrlClient + RtSafeCtrl + safety_
stop_fast 多 1.25 h，更真实）。

---

## §12 风险（v3/v4 R1-R10 保留 + v5 加）

### v5 R11：HardRT adapter 必须实现 `RtSafeCtrl` 的硬合约

**风险**：未来某个 adapter 实现方误标 `RtClass::HardRT` 但 `RtSafeCtrl` 实现
违约（藏了 alloc / sleep / lock）→ rt_thread tick 超时 / panic / 数据破坏。

**缓解**：
- trait `RtSafeCtrl` doc 明确 RT-safe 要求 + 加 `#[deny(unused_must_use)]` 让
  实现方意识到检查
- M2 加 `assert_no_alloc` 单测：`RtSafeCtrl` 方法在 `assert_no_alloc::assert_
  no_alloc(|| ...)` scope 下调用不 panic
- v0.4 真上 EtherCAT 时复审

### v5 R12：`safety_stop_fast()` 不等 ACK 的语义风险

**风险**：sync_write 广播是 fire-and-forget，**不等 ACK**。失败时上层不知道
硬件是否真断扭。

**缓解**：
- `serial_io_thread` 在 `safety_stop` 调用后**下一 tick** 调 `read_state` 读
  `Torque_Enable` 字段（dimos 既有）确认；不一致 → `OutputSafetyState::Stop
  Failed` + `FaultCode::AdapterUncertain`
- emergency 路径不阻塞等 ACK（速度优先）；冷路径（普通 `enable(false)`）保留
  可重试 + ACK
- 文档 §7.2 明确"`safety_stop_fast` 不保证硬件已断扭，只保证软件已下发"

---

## §13 v5 决议请求

请 review 并回复：

1. v5 整体（`ExecutorCtrlClient` trait + `OutputPolicy(AtomicU8)` + `RtSafe
   Ctrl` HardRT 合约 + `safety_stop_fast`）是否 approve？
2. **`ExecutorCtrlClient` trait 4 方法**（`set_policy` / `safety_stop` /
   `request_normal` / `poll_last_normal_resp`）是否覆盖完整？要不要加 `clear_
   pending` / `cancel` 等？
3. **`OutputPolicy` 用 `AtomicU8`** 而不是 `ArcSwap` —— 是否同意（保 RT 路径
   零 alloc / 零 hazard pointer）？
4. **`RtSafeCtrl` 子 trait** 强制 HardRT adapter 实现是否接受？v0.4 真上
   EtherCAT 时方便检查实现方？
5. **`FeetechAdapter::safety_stop_fast()`** 设计（sync_write broadcast id=0xFE
   + 不等 ACK + 1-2ms）是否同意？
6. `docs/10_verification.md:102` 状态改 "⏳ blocking gap (软件不可解；待
   task-30-post)" 同意？task-30 实施后此项 **仍 ⏳**。
7. M5 行为切换点（M6 内）是否拆两 commit（"flag on" + "删 legacy"）？
8. 真机最低门槛 M6 smoke：
   - mock_hard_rt 走 rt_thread 路径无回归（task 29 ±17° 双夹爪重跑通过）
   - feetech 走 serial_io_thread 路径：`mock_s1 --duration 0s` 后 `feetech_
     poke read Torque_Enable=0` + `robotctl status safety_state=TorqueOff
     Confirmed` + 物理推夹爪
   - **新增**：`mock_s1` 发 `ControlMode::Stop` → adapter 真断扭（feetech_poke
     在 mock_s1 还在跑时 `read Torque_Enable=0`，验证 Stop 路径走通）
   - 同意 / 加项？

review 通过我按 pre-1 → pre-2 → pre-3 → M1-M6 顺序动手 task-30 实施。
