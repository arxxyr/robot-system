# Task-30 设计方案：state_machine ↔ adapter 执行链路

> **状态**：**v7**，等再次 review
> **历史**：v1 (`4cca734`) → v2 (`130b44b`) → v3 (`f7bdc00`) → v4 (`881e927`)
> → v5 (`46dd3c1`) → v6 (`4d016d4`) → v7（user v6 六轮 review 后修订）
> **作者**：Claude Code
>
> **v7 关键修订**：v6 的 5 个真实卡点全收。最关键是 #1（HardRT 提交丢请求 +
> seq 误确认）：v6 的 `store + fetch_add` 有数据竞争 → 改 `compare_exchange`；
> #2（rt_thread 调 connect / disconnect / hold_current_position 都不是
> RT-safe）→ HardRT normal_action **只**编码 `RtSafeCtrl` 方法；
> #3（`sync_write_by_id` 内部仍 alloc）→ M4 加 `sync_write_by_id_into` 预分配
> frame API；#4（`AdapterUncertain` 自相矛盾）加 enum variant；
> #5（pre 任务无具体文件）§11.1 改写明确每个 pre 改哪些文件哪些行。

---

## §0 v6 → v7 修订记录

User v6 review 给出 5 项卡点，全部接受：

| # | v6 缺陷 | v7 修订位置 |
|---|---|---|
| **1** | `request_normal` 直接 `store(normal_action, action_code)` → 两个 normal 连续提交**静默覆盖**；`fetch_add` 返回旧 seq + rt_thread 完成时读"当前 action_seq"可能把**后**一个请求误标完成 | **§4.2 重写**：normal_action 用 `compare_exchange(NONE, action)` 占用 slot；占用失败 → `NormalReqStatus::Busy`；seq 在 cas 成功**之后**分配 `fetch_add(1) + 1`；rt_thread 完成时 seq + result 原子绑定 |
| **2** | rt_thread 主循环调 `adapter.connect()`（启动期方法）/ `disconnect()` / `hold_current_position()`（走 sync_read+write）—— 都不是 RT-safe | **§4.2 normal_action 编码集裁剪**：只允许 `RtSafeCtrl` 方法（`Enable(bool)` / `SetMode(Position)`）；`Connect` / `Disconnect` 移到 `s0_daemon` main（spawn 前 connect / join 后 disconnect）；`HoldCurrent` 在 HardRT 仅靠 `OutputPolicy::HoldCurrent` gate（rt_thread 不调 write_command = motor 维持上次 Goal）；SerialRT 才走 `hold_current_position()` 真 helper |
| **3** | `safety_stop_fast` 走 `sync_write_by_id` 但 `bus.rs:419/426/432` + `protocol.rs:201` 仍 alloc Vec → §2.19 "安全路径不分配"破产 | **§7.2 + M4** 加 `sync_write_by_id_into` 预分配 frame API（与现有 `sync_read_by_id_into` 对称），`build_sync_write_into(&mut frame_buf, ...)` 不 alloc；`safety_stop_fast` 走新 API；`assert_no_alloc` 单测覆盖 |
| **4** | `OutputSafetyState` enum 到 `StopFailed=4` 但 §6 引用 `AdapterUncertain` —— 自相矛盾 | **§5.3 加 `AdapterUncertain = 5`** variant；语义：safety_stop 下发**成功**但 confirm 读回 Torque_Enable mask **部分一致**（≠全断扭，≠全失败）；与 `StopFailed`（软件下发就失败）区别明确 |
| **5** | pre 任务无具体文件 / 行号 → 实施期发现 `RtClass::SerialRT` 不存在 / `RobotError::Unsupported` 不存在卡编译 | **§11.1 重写 pre 表**：每个 pre 列具体文件路径 + 当前代码状态 + 改动；含 `crates/robot-core/src/adapter.rs:31` (RtClass) / `crates/robot-adapters/feetech/src/adapter.rs:202` / `crates/robot-core/src/error.rs:28` / `joint.fbs:36` |

v1-v6 文档归档 git history。v7 是当前权威。

---

## §1 问题陈述（v4-v6 保留）

### 1.1 三类 adapter，三条 executor

| `RtClass` | 典型硬件 | RT 合规性 | Executor |
|---|---|---|---|
| `HardRT` | EtherCAT / RT-CAN / DMA mmap / mock_hard_rt | bounded latency + no alloc + no block + no syscall | `s0_rt_thread` (SCHED_FIFO) |
| `SerialRT` | 串口舵机 / USB-RS485 / Modbus RTU | 控制语义属 S0；IO ms 级 syscall | **新增** `s0_serial_io_thread` |
| `SoftRT` | RealSense / vendor SDK / 网络 sensor | 仅观测 | `s0_io_thread[0..N]` (v0.4) |

### 1.2 双重缺口（v3 已识别）

A：state_machine 不驱动 adapter；B：`latest_cmd` 无门控（`OutputPolicy` 解决）。

### 1.3 Schema / state_machine 修订（pre-2 修补）

`joint.fbs` 缺 `mode/control_link/output_mode/safety_state` 字段；
`state_machine` 缺 `* → Fault(SafetyStopFailed)` 转换。

### 1.4 SIGKILL ≠ task-30 完成

`docs/10_verification.md:102` "kill -9 → 硬件 fail-safe < 500 ms" **task-30
不闭环此 SLA**，留 task-30-post（servo timeout + 硬件继电器）。

---

## §2 设计原则（v6 19 条 + v7 加 1）

1-19：v6 保留。

**20（v7 新加）**：**RT slot 提交协议必须 1:1 配对**。提交端用 `compare_
exchange` 占用 slot 失败 → 返 `Busy`；seq 在 cas 成功后分配，与本次 action 原
子绑定；rt_thread 消费端先 `swap` 出 action 再 load seq → 保证"调用方持有的
seq" 与"rt_thread 完成的 seq" 一一对应，不串货。

---

## §3 架构图（v6 保留 + v7 修订 normal_action 协议）

```
┌─── robot-s0 ────────────────────────────────────────────────────────────┐
│                                                                         │
│  ipc_thread (state_machine)                                             │
│  ┌──────────────────────────────────────┐                               │
│  │ executor_ctrl: Box<dyn ExecutorCtrl> │                               │
│  │ on transition:                       │                               │
│  │   set_policy / safety_stop /         │                               │
│  │   request_normal (返 Submitted/Busy) │                               │
│  └──────────────┬───────────────────────┘                               │
│                 │ trait                                                 │
│  ╔══════════════▼═══════════════════════════════════════════════════╗  │
│  ║                                                                  ║  │
│  ║  HardRtCtrlClient impl (全 atomic)  SerialIoCtrlClient impl     ║  │
│  ║  ┌──────────────────────────────┐   ┌────────────────────────┐  ║  │
│  ║  │ policy: AtomicU8             │   │ policy: AtomicU8       │  ║  │
│  ║  │ emergency_action: AtomicU8   │   │ emergency_tx: ch (8)   │  ║  │
│  ║  │ normal_action: AtomicU8      │   │ estop_atomic: AB       │  ║  │
│  ║  │   (CAS occupy / Busy reject) │   │ pending_normal: ArcSwap│  ║  │
│  ║  │ action_arg: AtomicU8         │   │ last_resp:    ArcSwap  │  ║  │
│  ║  │ action_seq: AtomicU64        │   │                        │  ║  │
│  ║  │   (cas 成功后 fetch_add+1)    │   │                        │  ║  │
│  ║  │ last_completed_seq: AU64     │   │                        │  ║  │
│  ║  │ last_completed_result: AU8   │   │                        │  ║  │
│  ║  │ safety_latched: AtomicBool   │   │                        │  ║  │
│  ║  └──────────┬───────────────────┘   └────────────┬───────────┘  ║  │
│  ║             ▼                                    ▼              ║  │
│  ║  ┌──────────────────────────────┐   ┌────────────────────────┐  ║  │
│  ║  │ rt_thread (SCHED_FIFO)        │   │ serial_io_thread        │  ║  │
│  ║  │ tick @ 50Hz：                 │   │ select! {                │  ║  │
│  ║  │ 1. emergency_action.swap     │   │   recv emergency → exec  │  ║  │
│  ║  │    → safety_stop_rt          │   │   pending normal         │  ║  │
│  ║  │    → safety_latched=true     │   │   default(20ms) → tick:  │  ║  │
│  ║  │ 2. normal_action.swap        │   │     check estop_atomic   │  ║  │
│  ║  │    → enable_rt / set_mode_rt │   │     按 OutputPolicy gate │  ║  │
│  ║  │    （仅 RtSafeCtrl 方法）     │   │     if StopCommanded:    │  ║  │
│  ║  │    → load action_seq         │   │       confirm_torque_off │  ║  │
│  ║  │    → store last_completed_*  │   │     adapter.write/read   │  ║  │
│  ║  │ 3. policy gate (ActiveForward│   │ }                        │  ║  │
│  ║  │    + !latched → write_cmd)   │   │                          │  ║  │
│  ║  │ 4. read_state                │   │  独占 SerialRT adapter   │  ║  │
│  ║  │ 5. publish snapshot          │   │  + OutputSafetyProbe     │  ║  │
│  ║  │ 全程零 alloc / 零 Arc        │   └──────────────────────────┘  ║  │
│  ║  │ 不调 connect/disconnect/hold │                                 ║  │
│  ║  │ （这些走 main 启动 / 退出）   │                                 ║  │
│  ║  └──────────────────────────────┘                                 ║  │
│  ╚══════════════════════════════════════════════════════════════════╝  │
│                                                                         │
│  HotShared.safety_state_slot (AtomicU8 编码 OutputSafetyState 6 variant) │
│  pub_thread reads → state topic safety_state 字段 (pre-2 schema)        │
└─────────────────────────────────────────────────────────────────────────┘
```

---

## §4 ExecutorCtrlClient trait（v5-v6 保留 + v7 修 §4.2）

### 4.1 trait 定义（v5 保留）

```rust
pub trait ExecutorCtrlClient: Send + Sync {
    fn set_policy(&self, policy: OutputPolicy);
    fn safety_stop(&self);
    fn request_normal(&self, req: AdapterNormalReq) -> NormalReqStatus;
    fn poll_last_normal_resp(&self, expected_seq: u64) -> Option<AdapterResp>;
}

#[derive(Clone, Copy, Debug)]
pub enum NormalReqStatus {
    /// 占用 slot 成功，调用方持有 `seq`，下次用 `poll_last_normal_resp(seq)` 查结果
    Submitted { seq: u64 },
    /// **v7 新增**：slot 已被上一个 normal action 占用，未被 rt_thread 消费；
    /// 调用方应稍后重试或决定是否抢占
    Busy,
    /// 编码失败（复杂参数 unsupported）
    Unsupported(&'static str),
}
```

### 4.2 HardRT 实现：v7 重写（CAS 占用 + RtSafeCtrl-only 编码）

```rust
/// HardRT executor 的 ctrl client。**所有字段都是 atomic** —— rt_thread 主
/// 循环零 alloc / 零 Arc refcount。
pub struct HardRtCtrlInner {
    policy: AtomicU8,

    /// 紧急 action：rt_thread tick **优先**消费。1 = SafetyStop。
    emergency_action: AtomicU8,

    /// **v7 修**：normal action **CAS 占用**协议：
    /// - `request_normal` 用 `compare_exchange(NONE, action)` 占用
    /// - 占用失败（slot != NONE）→ 返 `Busy`，调用方知晓上次未消费
    /// - rt_thread 用 `swap(NONE)` 消费
    /// **v7 编码集裁剪**（仅 `RtSafeCtrl` 方法）：
    /// - 1 = Enable(bool)（arg = 0/1）
    /// - 2 = SetMode(Position)（arg 忽略；v0.2 仅 Position）
    /// **不再编码** Connect / Disconnect / HoldCurrent（移到 main / OutputPolicy）
    normal_action: AtomicU8,
    action_arg: AtomicU8,

    /// `request_normal` cas 成功后 `fetch_add(1) + 1` → `Submitted { seq }`
    /// 返调用方；rt_thread 完成后 `last_completed_seq.store(seq)` + result
    action_seq: AtomicU64,
    last_completed_seq: AtomicU64,
    last_completed_result: AtomicU8,
    /// 标记 last_completed_seq 写入完成（防 result 写入未 visible 时被读）
    last_completed_ready: AtomicBool,

    /// EStop / Fault 后置位；rt_thread 拒绝 normal Enable(true)；ClearEStop 清
    safety_latched: AtomicBool,
}

pub struct HardRtCtrlClient {
    inner: Arc<HardRtCtrlInner>, // spawn 时一次 clone，rt_thread 持只读引用
}

impl ExecutorCtrlClient for HardRtCtrlClient {
    fn set_policy(&self, policy: OutputPolicy) {
        self.inner.policy.store(policy as u8, Ordering::Release);
    }

    fn safety_stop(&self) {
        self.inner.emergency_action.store(EMERGENCY_SAFETY_STOP, Ordering::AcqRel);
    }

    fn request_normal(&self, req: AdapterNormalReq) -> NormalReqStatus {
        // v7 §0 修订 #2：HardRT 只编码 RtSafeCtrl 方法
        let (action_code, arg) = match req {
            AdapterNormalReq::Enable(true) => (NORMAL_ENABLE, 1),
            AdapterNormalReq::Enable(false) => (NORMAL_ENABLE, 0),
            AdapterNormalReq::SetMode(ControlMode::Position) => (NORMAL_SET_MODE_POSITION, 0),
            AdapterNormalReq::Connect
            | AdapterNormalReq::Disconnect => {
                // 走 main 路径：spawn 前 connect / join 后 disconnect
                return NormalReqStatus::Unsupported(
                    "HardRT path: Connect/Disconnect must be called from s0_daemon main, not rt_thread"
                );
            }
            AdapterNormalReq::HoldCurrentPosition => {
                // HardRT path: HoldCurrent 由 OutputPolicy::HoldCurrent gate 实现（不调 write_command）
                return NormalReqStatus::Unsupported(
                    "HardRT path: HoldCurrent realized via OutputPolicy gate, no rt action"
                );
            }
            AdapterNormalReq::SetMode(other) => {
                // v0.2 HardRT 仅支持 Position mode；其他模式留 v0.3+
                return NormalReqStatus::Unsupported(
                    "HardRT path: only ControlMode::Position supported in v0.2"
                );
            }
        };

        // safety_latched 时拒绝 Enable(true)
        if action_code == NORMAL_ENABLE && arg == 1
            && self.inner.safety_latched.load(Ordering::Acquire)
        {
            return NormalReqStatus::Unsupported(
                "Enable(true) refused while safety latched (need ClearEStop first)"
            );
        }

        // **v7 关键**：先 store arg（rt_thread 看到 normal_action 才会读 arg）
        self.inner.action_arg.store(arg, Ordering::Release);

        // CAS 占用 normal_action slot
        match self.inner.normal_action.compare_exchange(
            NORMAL_NONE,
            action_code,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                // 占用成功 → 分配新 seq
                let new_seq = self.inner.action_seq.fetch_add(1, Ordering::AcqRel) + 1;
                NormalReqStatus::Submitted { seq: new_seq }
            }
            Err(_) => {
                // 上一个 normal 还没被 rt_thread 消费
                NormalReqStatus::Busy
            }
        }
    }

    fn poll_last_normal_resp(&self, expected_seq: u64) -> Option<AdapterResp> {
        if !self.inner.last_completed_ready.load(Ordering::Acquire) {
            return None;
        }
        let last_seq = self.inner.last_completed_seq.load(Ordering::Acquire);
        if last_seq != expected_seq {
            return None; // 旧请求结果或还没执行
        }
        let code = self.inner.last_completed_result.load(Ordering::Acquire);
        Some(decode_resp(code))
    }
}
```

**rt_thread 主循环（v7 重写）**：

```rust
fn run_rt_loop<A: ManipulatorAdapter + RtSafeCtrl>(
    cfg: RtThreadConfig,
    mut adapter: A,                          // **已 connect**（main 在 spawn 前 connect）
    ctrl: Arc<HardRtCtrlInner>,
    cmd_reader: ...,
    snapshot_pub: ...,
    safety_state_slot: Arc<AtomicU8>,
    ...
) {
    // pre-warmup hazard pointer 等
    loop {
        let deadline = ...;
        clock.sleep_until(deadline);
        rt_entry!({
            // 1. emergency 优先消费
            let em = ctrl.emergency_action.swap(EMERGENCY_NONE, Ordering::AcqRel);
            if em == EMERGENCY_SAFETY_STOP {
                let result = adapter.safety_stop_rt();
                ctrl.safety_latched.store(true, Ordering::Release);
                // HardRT mmap 写完即生效，TorqueOffConfirmed 直接置（不需 confirm path）
                let s = match result {
                    Ok(()) => OUTPUT_SAFETY_TORQUE_OFF_CONFIRMED,
                    Err(_) => OUTPUT_SAFETY_STOP_FAILED,
                };
                safety_state_slot.store(s, Ordering::Release);
            }

            // 2. normal action（仅 RtSafeCtrl 方法）
            let na = ctrl.normal_action.swap(NORMAL_NONE, Ordering::AcqRel);
            if na != NORMAL_NONE {
                let arg = ctrl.action_arg.load(Ordering::Acquire);
                // **关键**：先 load action_seq（消费时刻 = 提交时分配的 seq）
                let seq = ctrl.action_seq.load(Ordering::Acquire);
                let result = match na {
                    NORMAL_ENABLE => adapter.enable_rt(arg == 1),
                    NORMAL_SET_MODE_POSITION => adapter.set_mode_rt(ControlMode::Position),
                    _ => Err(RobotError::Unsupported("rt_thread: unknown normal_action")),
                };
                let code = encode_resp(&result);
                // 顺序：result → seq → ready，让 client 看到 ready=true 时
                // 一定能读到匹配的 seq + result
                ctrl.last_completed_result.store(code, Ordering::Release);
                ctrl.last_completed_seq.store(seq, Ordering::Release);
                ctrl.last_completed_ready.store(true, Ordering::Release);
                // 成功 Enable(true) 后清 safety_latched（让后续 normal 可发）
                if matches!(result, Ok(())) && na == NORMAL_ENABLE && arg == 1 {
                    ctrl.safety_latched.store(false, Ordering::Release);
                }
            }

            // 3. policy gate
            let policy_u8 = ctrl.policy.load(Ordering::Acquire);
            let policy = OutputPolicy::from_u8(policy_u8);
            // ActiveForward 且 !safety_latched 才转发
            if matches!(policy, OutputPolicy::ActiveForward)
                && !ctrl.safety_latched.load(Ordering::Acquire)
            {
                let cmd_guard = cmd_reader.load();
                let cmd = cmd_guard.as_view();
                let _ = adapter.write_command(&cmd);
            }
            // HoldCurrent / Disabled / StopTorque / ReadyHold / ZeroCommand 都是
            // "不调 write_command"；motor 输出层面：torque off 由 emergency
            // safety_stop_rt 或 normal Enable(false) 单独驱动；hold 由"不发新
            // Goal" 隐式实现（HardRT motor 维持上次 Goal = 当前 Present 附近）

            // 4. read_state + publish
            let _ = adapter.read_state(&mut state_buf);
            snapshot_pub.publish(...);
        });
    }
}
```

**编码常量（reference）**：

```rust
// emergency_action
pub const EMERGENCY_NONE: u8 = 0;
pub const EMERGENCY_SAFETY_STOP: u8 = 1;

// normal_action（仅 RtSafeCtrl 方法）
pub const NORMAL_NONE: u8 = 0;
pub const NORMAL_ENABLE: u8 = 1;
pub const NORMAL_SET_MODE_POSITION: u8 = 2;

// last_completed_result
pub const RESP_OK: u8 = 0;
pub const RESP_ERR_INVALID_PARAM: u8 = 1;
pub const RESP_ERR_HARDWARE_FAULT: u8 = 2;
pub const RESP_ERR_OTHER: u8 = 3;
```

### 4.3 SerialRT 实现（v5 保留）

`SerialIoCtrlClient` 用 emergency channel + AtomicBool 兜底 + ArcSwap pending
normal —— 普通线程允许 ArcSwap。详 v5 §4.3。SerialRT 路径**保留**`Connect /
Disconnect / HoldCurrent` action（serial_io_thread 普通线程允许这些 ms 级 IO）。

---

## §5 OutputPolicy / OutputSafetyState / OutputSafetyProbe / RtSafeCtrl

### 5.1 OutputPolicy（v6 保留）

6 variant + `from_u8` 6 valid + 兜底 `Disabled`。详 v6 §5.1。

### 5.2 OutputPolicy 在两类 executor 的实现

| Policy | HardRT (rt_thread) | SerialRT (serial_io_thread) |
|---|---|---|
| `Disabled` | 不调 `write_command`；torque 状态由 emergency `SafetyStop` 或 normal `Enable(false)` 单独驱动（与 v6 一致） | 同 |
| `ReadyHold` | 不调 `write_command`（adapter 已 connected + Position + torque off） | 同 |
| `ActiveForward` | 调 `write_command(cmd)`；要求 RT-safe（HardRT adapter 合约）；`cmd.mode = Stop` → adapter 内 RT-safe cut output（合约 §5.5） | 调 `write_command(cmd)`；含 `cmd.mode = Stop` → `executor_ctrl.safety_stop()` emergency；非 Position → `Err(Unsupported)` Fault |
| `HoldCurrent` | **不调 write_command**（motor 维持上次 Goal）—— v0.2 范围内对 mock_hard_rt 等价 hold；真 HardRT EtherCAT 等可以加 `RtSafeCtrl::hold_current_rt`（v0.4 评估） | 调 `adapter.hold_current_position()`；失败 fallback `safety_stop` |
| `ZeroCommand` | 不调 `write_command`（v0.2 退化为 hold） | 同 |
| `StopTorque` | 不调 `write_command`；torque off 由 `safety_stop_rt` 或 `enable_rt(false)` 驱动 | 同 |

### 5.3 OutputSafetyState（**v7 加 `AdapterUncertain`**）

```rust
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputSafetyState {
    TorqueUnknown        = 0,
    TorqueOn             = 1,
    StopCommanded        = 2,  // safety_stop 已下发，未 confirm（SerialRT 用）
    TorqueOffConfirmed   = 3,
    StopFailed           = 4,  // 下发就失败
    /// **v7 新增**：下发**成功**但 confirm 读回 Torque_Enable mask **部分一致**
    /// （≠ 全断扭，≠ 全失败）。状态明确不一致；上报 `FaultCode::AdapterUncertain`。
    AdapterUncertain     = 5,
}
```

`from_u8`：`0..=5` 直映射，其他 → `TorqueUnknown` 兜底。

### 5.4 OutputSafetyProbe trait（v6 保留）

详 v6 §5.4。Feetech impl 走 sync_read `Torque_Enable`。Mock impl 返回内部
`enabled` 镜像。

### 5.5 RtSafeCtrl trait（v6 保留 + v7 编码集对齐）

```rust
pub trait RtSafeCtrl {
    /// RT-safe 断扭。bounded < 1 µs（mmap 写寄存器即返）。
    fn safety_stop_rt(&mut self) -> Result<(), RobotError>;

    /// RT-safe enable / disable。不带防飞序列（防飞由 OutputPolicy + read_state
    /// follow-current 实现）。
    fn enable_rt(&mut self, on: bool) -> Result<(), RobotError>;

    /// RT-safe 模式切换（仅瞬时寄存器写）。v0.2 仅要求 Position 真支持。
    fn set_mode_rt(&mut self, mode: ControlMode) -> Result<(), RobotError>;

    // write_command(mode = Stop) 必须 RT-safe cut output（trait `Manipulator
    // Adapter::write_command` 主签名的合约附加；不在 RtSafeCtrl 显式列方法）
}
```

**实施方**：
- `MockAdapter` (RtClass::HardRT) **实现** `RtSafeCtrl`（trivially RT-safe，
  状态字段写）；M2 加 `assert_no_alloc` 单测验证
- `FeetechAdapter` (RtClass::SerialRT) **不实现** `RtSafeCtrl`；编译期
  `spawn_rt_thread<A: ManipulatorAdapter + RtSafeCtrl>` trait bound 拒错配

---

## §6 SerialRT confirm_torque_off 路径（v6 保留 + v7 加 AdapterUncertain）

```rust
fn tick_hardware(...) {
    // ... v6 §6 保留 ...

    if state.safety_state == OutputSafetyState::StopCommanded {
        let n = state.joint_count;
        let mask = &mut state.confirm_buf[..n];
        match probe.read_torque_enable_mask(mask) {
            Ok(()) => {
                let all_off = mask.iter().all(|&b| !b);
                let any_off = mask.iter().any(|&b| !b);
                state.safety_state = if all_off {
                    OutputSafetyState::TorqueOffConfirmed
                } else if any_off {
                    // **v7 fix #4**：部分 motor 仍 torque on
                    let _ = ctx.fault_tx.try_send(FaultCode::AdapterUncertain);
                    OutputSafetyState::AdapterUncertain
                } else {
                    // 全 motor 仍 torque on（safety_stop_fast 完全失效）
                    let _ = ctx.fault_tx.try_send(FaultCode::SafetyStopFailed);
                    OutputSafetyState::StopFailed
                };
            }
            Err(e) => {
                tracing::error!(?e, "confirm_torque_off failed");
                state.safety_state = OutputSafetyState::StopFailed;
                let _ = ctx.fault_tx.try_send(FaultCode::SafetyStopFailed);
            }
        }
        ctx.safety_state_slot.store(state.safety_state as u8, Ordering::Release);
    }
}
```

---

## §7 Feetech `safety_stop_fast` + 预分配 frame API（v7 修 #3）

### 7.1 当前状态

`enable(false)` 13 motor × 3 retry × 150ms = ~5.85s 最坏。

### 7.2 v7 实现：新增 `sync_write_by_id_into` 预分配 API

**问题**：v6 调 `bus.sync_write_by_id` 但 `bus.rs:419/426/432` + `protocol.rs:201`
`build_sync_write` 内部仍 alloc Vec —— "预分配 buffer" 承诺破产。

**v7 修订**：M4 加 `sync_write_by_id_into` 预分配 frame API：

```rust
// crates/robot-adapters/feetech/src/bus.rs
impl<T: SerialTransport> FeetechBus<T> {
    /// 预分配 frame buffer 版本的 sync_write。复用调用方持有的 `frame_buf`，
    /// 不在内部 `Vec::new()` / `Vec::push`。
    ///
    /// `frame_buf` 必须 `with_capacity(MAX_FRAME_LEN)` 预分配；本方法
    /// `clear()` 后 `extend_from_slice` —— 容量足够则不 realloc。
    /// 与 `sync_read_by_id_into` 对称。
    pub fn sync_write_by_id_into(
        &mut self,
        reg_name: &'static str,
        entries: &[(u8, i32)],
        frame_buf: &mut Vec<u8>,
    ) -> Result<(), FeetechError> {
        frame_buf.clear();
        // build_sync_write_into 写入 frame_buf 不 alloc（capacity 已够）
        crate::protocol::build_sync_write_into(reg_name, entries, frame_buf)?;
        self.transport.write_all(frame_buf)?;
        // sync_write 不等 ACK
        Ok(())
    }
}

// crates/robot-adapters/feetech/src/protocol.rs
pub fn build_sync_write_into(
    reg_name: &'static str,
    entries: &[(u8, i32)],
    out: &mut Vec<u8>,
) -> Result<(), FeetechError> {
    let reg = get_register(reg_name)?;
    let n = entries.len();
    let len = (n * (1 + reg.size as usize)) + 4; // PARAMS = addr + size + N×(id+data)
    out.push(0xFF);
    out.push(0xFF);
    out.push(0xFE); // broadcast id
    out.push(len as u8);
    out.push(0x83); // SYNC_WRITE
    out.push(reg.addr);
    out.push(reg.size);
    for &(id, val) in entries {
        out.push(id);
        // val 编码到 reg.size 字节（sign-magnitude 等，复用现有 encoding）
        encode_value_into(reg_name, val, reg.size, out)?;
    }
    let chk = checksum(&out[2..]);
    out.push(chk);
    Ok(())
}
```

`FeetechAdapter`：

```rust
pub struct FeetechAdapter<T: SerialTransport> {
    // ... 现有字段 ...
    safety_stop_buf: Vec<(u8, i32)>,    // joint_count × (id, 0)
    safety_stop_frame_buf: Vec<u8>,     // MAX_FRAME_LEN bytes
    confirm_torque_buf: Vec<i32>,
}

impl FeetechAdapter {
    pub fn safety_stop_fast(&mut self) -> Result<(), FeetechError> {
        // safety_stop_buf 已预填 (id, 0)；这里只更新值确保都是 0
        for entry in self.safety_stop_buf.iter_mut() {
            entry.1 = 0;
        }
        // 用预分配 frame_buf，运行期不 alloc
        self.bus.sync_write_by_id_into(
            "Torque_Enable",
            &self.safety_stop_buf,
            &mut self.safety_stop_frame_buf,
        )?;
        Ok(())
    }

    pub fn connect(&mut self) -> Result<(), RobotError> {
        // ... 现有 handshake ...

        // v7 预分配
        self.safety_stop_buf.clear();
        for &id in &self.joint_ids {
            self.safety_stop_buf.push((id, 0));
        }
        // MAX_FRAME_LEN：N joint × 3 byte (id + 2 byte data) + 8 header/checksum = 13×3+8 ≈ 47
        // 保守 256 字节够用
        self.safety_stop_frame_buf = Vec::with_capacity(256);
        self.confirm_torque_buf.resize(self.joint_ids.len(), 0);
        Ok(())
    }
}
```

**测试**（M4）：
- mock transport：`safety_stop_fast` 调用前后 `Vec` capacity 不变（无 realloc）
- `assert_no_alloc::assert_no_alloc(|| adapter.safety_stop_fast())` 不 panic
- wire bytes 含 broadcast id `0xFE` + `Torque_Enable` addr + 全 0 data
- 时延 ≤ 5ms（mock transport 即时返回）

---

## §8 SIGTERM graceful + SIGKILL blocking gap（v5 保留）

详 v5 §9。

---

## §9 测试覆盖（v6 + v7）

继承 v6 全部 + v7 加：

| 用例 | 验证 |
|---|---|
| `request_normal` 两次连续提交 → 第二次返 `Busy`（不覆盖第一个）| v7 #1 |
| `request_normal` cas 成功后 seq + action 1:1 配对（rt_thread swap action 后 load seq 与 client 持有 seq 一致）| v7 #1 |
| `request_normal(Connect / Disconnect / HoldCurrent / SetMode(Velocity))` 在 HardRT 路径返 `Unsupported` | v7 #2 |
| rt_thread 主循环不 reach `adapter.connect/disconnect/hold_current_position` | v7 #2 |
| `sync_write_by_id_into` 用预分配 frame_buf；mock transport 验证 wire bytes + capacity 不变 | v7 #3 |
| `safety_stop_fast` 在 `assert_no_alloc::assert_no_alloc(...)` scope 不 panic | v7 #3 + R11 |
| `confirm_torque_off` mask 部分 1 部分 0 → `OutputSafetyState::AdapterUncertain` + `FaultCode::AdapterUncertain` | v7 #4 |
| `OutputSafetyState::from_u8(5)` → `AdapterUncertain` | v7 #4 |

---

## §10 风险（v3-v6 + v7 加 R15）

### v7 R15：HardRT executor `OutputPolicy::HoldCurrent` 隐式 hold 假设

**风险**：v7 `OutputPolicy::HoldCurrent` 在 HardRT 路径**不调 write_command**，
依赖 motor 维持上次 Goal。如果 HardRT motor 是 "无 Goal 不维持"（少见），会
drift 或 disable。

**缓解**：
- 真 HardRT adapter 的 motor 几乎都内部 hold last Goal（EtherCAT servo / RT-CAN
  motor controller / mock_hard_rt 仿真）；这个假设对 v0.4+ 真上 EtherCAT 时
  仍要确认 servo 行为
- v0.4 真上 EtherCAT 时若 motor "无 Goal 不维持" → 加 `RtSafeCtrl::hold_
  current_rt`（瞬时写一个 Goal=Present 的 PDO），rt_thread `OutputPolicy::
  HoldCurrent` 时调
- v0.2 mock_hard_rt 模拟即可

---

## §11 迁移计划（v7 重写 pre 表 + 改进 M 表）

### 11.1 前置 task-30-pre（**v7 列具体文件**）

| # | 主题 | 具体文件 + 当前状态 + 改动 | 工作量 | 评审 |
|---|---|---|---|---|
| **pre-1a** | `RtClass` 加 `SerialRT` variant | `crates/robot-core/src/adapter.rs:31-37` 当前 `enum RtClass { HardRT, SoftRT }` → 加 `SerialRT`；`Display` impl 同步；`from_str` 同步；单测扩 | 15 min | §4.6 设计修订 |
| **pre-1b** | Feetech adapter rt_class HardRT → SerialRT | `crates/robot-adapters/feetech/src/adapter.rs:202` 当前 `RtClass::HardRT` → `RtClass::SerialRT`；删除 v0.2 task 28 注释 "目标分类" 一段；改 `info()` 文档说明 | 5 min | 与 pre-1a 同评审 |
| **pre-1c** | `docs/00_design.md` §4.6 Adapter 分级章节扩 | 加 `SerialRT` 段：定义、判定准则（可证明 RT 合规性）、典型硬件（串口舵机 / USB-RS485 / Modbus RTU）；走 dimos 同步回写 `docs/09_robot_framework.md` | 30 min | **设计修订评审** |
| **pre-1d** | `docs/10_verification.md` 加 SerialRT baseline 行 | 当前 §10.1 只有 HardRT / SoftRT；加 SerialRT：`feetech_poke ping 13-motor` baseline、`safety_stop_fast` 时延 SLA `< 10ms`、confirm_torque_off RTT SLA `< 50ms` | 15 min | 同 pre-1c |
| **pre-2** | state schema 补字段 + state_machine 转换表加 `* → Fault(SafetyStopFailed)` | `crates/robot-msg/schemas/joint.fbs:36` 加字段（`mode/control_link/output_mode/safety_state` 全 `Unknown` variant 兼容旧 writer）；`crates/robot-s0/src/state_machine.rs:357` 加全状态转换；`build_helpers` + `pub_thread:237` + `robotctl status` + `schema_compat` 测试 | 1.5 h | **§3.4 / §1.4 设计修订** |
| **pre-3a** | `RobotError::Unsupported(&'static str)` variant + thiserror Display | `crates/robot-core/src/error.rs:28` 当前 enum 无 Unsupported；加 variant + `#[error("unsupported: {0}")]` + `impl From` 链路；现有 `Err(RobotError::InvalidParameter(...))` 不动 | 15 min | 纯增量 |
| **pre-3b** | adapter_io_thread 走 RobotClock | `crates/robot-core/src/time.rs` 现成 `MonoRawNs` / `Deadline` / `RobotClock` trait；serial_io_thread 内部用 `RobotClock` 替换任何 `std::time::Instant::now`（业务代码 §业务代码禁用 API） | 30 min | 纯增量 |

### 11.2 主体 M1-M7（v6 保留 + v7 修 M4）

| # | 主题 | 工作量 | 依赖 |
|---|---|---|---|
| **M1** | `ExecutorCtrlClient` trait + `OutputPolicy(AtomicU8)` + `OutputSafetyState(含 StopCommanded + AdapterUncertain)` + `AdapterNormalReq` + `NormalReqStatus(Submitted/Busy/Unsupported)` + atomic 编码常量；纯类型 + 单测（含 `Busy` semantics + `AdapterUncertain` u8 转换） | 1 h | pre-3a/b |
| **M2** | `RtSafeCtrl` 子 trait + `MockAdapter` 实现 + `assert_no_alloc` 单测；`ManipulatorAdapter::write_command(mode=Stop)` 合约 doc | 1 h | M1 |
| **M3** | `OutputSafetyProbe` trait + `FeetechAdapter` 实现 + `MockAdapter` 实现；mock transport 单测 | 1 h | pre-1 |
| **M4** | **v7 重写**：`bus.sync_write_by_id_into` + `protocol.build_sync_write_into` 预分配 frame API；`FeetechAdapter::safety_stop_fast()` 走新 API；`safety_stop` override + 预分配 buffer；`assert_no_alloc` 单测 + wire bytes 单测 + 时延单测 | 1.5 h | M3 |
| **M5** | `HardRtCtrlClient` 实现（**全 atomic + CAS 占用 normal_action**）+ rt_thread 主循环消费 emergency / normal action + safety_latched 拒绝 enable + `assert_no_alloc` soak（rt_guard_soak 复用） | 1.5 h | M1 / M2 |
| **M6** | `SerialIoCtrlClient` + `serial_io_thread` skeleton + `select!` loop + `tick_hardware` 含 `confirm_torque_off` 路径（StopCommanded → TorqueOffConfirmed / AdapterUncertain / StopFailed 三分支）；feature flag off | 2.5 h | M1 / M3 / M4 / pre-3b |
| **M7** | `s0_daemon` dispatch by `RtClass`（含 main 路径调 connect/disconnect 不走 rt_thread）；ipc_thread 加 `Box<dyn ExecutorCtrlClient>` + transition hook 真驱动；删 legacy bootstrap；feature flag default on；smoke 通过下个 release 删 flag | 1.5 h | M5 / M6 / pre-2 |

**总 ~10 h**（v6 估 9.5 h；v7 加 `sync_write_by_id_into` API + `Busy` 语义 + 测试增强多 0.5 h）。

---

## §12 v7 决议请求

请 review 并回复：

1. v7 整体（CAS occupy + RtSafeCtrl-only normal_action + `sync_write_by_id_into` 预分配 + `AdapterUncertain` variant + pre 任务列文件）approve？
2. **`NormalReqStatus::Busy`** 语义：调用方收到后**应**重试还是抢占？v7 让调用方决定，state_machine transition hook 收 Busy 时直接放弃本次 request 等下次 transition 重试 —— 同意？
3. **HardRT `Connect / Disconnect / HoldCurrent` 不走 rt_thread**：`Connect/Disconnect` 移到 `s0_daemon` main；`HoldCurrent` 由 `OutputPolicy::HoldCurrent` gate 隐式实现（不调 write_command = motor 维持 last Goal）。同意？
4. **R15 假设**："HardRT motor 默认 internal hold last Goal" 对 mock_hard_rt 适用；v0.4 真上 EtherCAT 时若不适用则加 `RtSafeCtrl::hold_current_rt`。同意？
5. **`sync_write_by_id_into` API 与现有 `sync_read_by_id_into` 对称**（M4 加），改 protocol 内 `build_sync_write_into` 接受 `&mut Vec<u8>`。同意 / 改名 / 改签名？
6. pre 任务表（pre-1a/b/c/d + pre-2 + pre-3a/b）列具体文件 + 行号是否清晰可执行？
7. M7 行为切换点拆两 commit（"flag on" + "删 legacy"）？
8. 真机最低门槛 M7 smoke：v6 + v7 加 `request_normal` `Busy` semantics 测（mock 提交两次第二次返 Busy）+ feetech `safety_stop_fast` `assert_no_alloc` + `AdapterUncertain` 路径测（mock 强制 confirm 部分一致）。同意 / 加项？

review 通过我按 pre-1 → pre-2 → pre-3 → M1-M7 顺序动手 task-30 实施。
