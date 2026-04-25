# Task-30 设计方案：state_machine ↔ adapter 执行链路

> **状态**：**v6**，等再次 review
> **历史**：v1（`4cca734`）→ v2（`130b44b`）→ v3（`f7bdc00`）→ v4（`881e927`）
> → v5（`46dd3c1`，706 行）→ v6（本版本，user v5 五轮 review 后修订）
> **作者**：Claude Code
>
> **v6 关键修订**：v5 三个 blocker + 四个修正全收。最重要的是 **B1 ("下发"≠
> "确认")**：`safety_stop_fast` 不等 ACK 的语义必须诚实反映到 `OutputSafety
> State`，不能立刻标 `TorqueOffConfirmed`。**B2 (确认通路缺失)**：现有
> `JointStateBuf` / `read_state` 没有 `Torque_Enable` 字段；**新增独立
> `OutputSafetyProbe` trait**，不污染普通 read_state 路径。**B3 (RT 路径
> ArcSwap)**：HardRT 路径所有控制动作全 atomic 编码，杜绝 rt_thread 碰
> Arc refcount。
>
> 同时修：emergency / normal action 拆两个 atomic（避免 SafetyStop 被
> Enable(true) 覆盖）；RtSafeCtrl 实施方修正（mock 实现 / Feetech 不实现）；
> `safety_stop_fast` 用 adapter 内预分配 buffer（避免内存压力下急停路径分配
> 失败）。

---

## §0 v5 → v6 修订记录

User v5 review 给出 3 blocker + 4 修正，全部接受：

### 3 Blocker

| # | v5 缺陷 | v6 修订位置 |
|---|---|---|
| **B1** | `safety_stop_fast` 成功立刻标 `TorqueOffConfirmed`；但 fast path 不等 ACK 只保证"软件已下发"≠"硬件已断扭" | **§5.4 + §7.3** `OutputSafetyState` 加 **`StopCommanded`** variant：下发成功 → `StopCommanded`；下一 tick `confirm_torque_off()` 读回 → 一致升 `TorqueOffConfirmed`，不一致 → `AdapterUncertain`，读失败 → `StopFailed` |
| **B2** | "下一 tick read_state 读回 Torque_Enable" 但 `JointStateBuf` 没此字段；接口不支持 | **§5.3 新增 `OutputSafetyProbe` trait**（独立路径，不污染普通 `read_state` / `JointStateBuf`）：`fn read_torque_enable_mask(&mut self, out: &mut [bool]) -> Result<(), RobotError>`；Feetech impl 走 sync_read `Torque_Enable` 寄存器；序列化加 `safety_state_slot`，不入 state topic 主 schema |
| **B3** | HardRT 路径 `ArcSwap<Option<AdapterNormalReq>>` 冷字段被 rt_thread 触发 `load_full` → Arc refcount → **违反 §4.9 RT 铁律** | **§4.2 重写 `HardRtCtrlInner`**：所有控制动作全 atomic（`emergency_action: AtomicU8` / `normal_action: AtomicU8` / `action_arg: AtomicU8` / `action_seq: AtomicU64`）；**ArcSwap 完全移除 HardRT 路径**；不可 atomic 编码的请求（如 SetMode 含复杂参数）→ HardRtCtrlClient 返 `Unsupported`，不入 RT loop |

### 4 修正

| # | v5 错误 | v6 修订 |
|---|---|---|
| **C1** | `action_slot` 单 atomic last-writer-wins → SafetyStop 被随后 Enable(true) 覆盖 = **危险** | **§4.2 拆 emergency_action + normal_action 两 atomic**；rt_thread tick 永远**先消费 emergency**；safety latched 后**拒绝 normal Enable(true)**（state_machine 必须先 ClearEStop） |
| **C2** | `OutputPolicy::from_u8` 文档说"全 8 值"，enum 只 6 variant | **§5.1 改措辞**："`from_u8` 接受 6 valid + 兜底 `Disabled`（保守）" |
| **C3** | "Mock + Feetech 不实现 `RtSafeCtrl`" 自相矛盾（mock 是 HardRT 必须实现） | **§5.5 改正**：Mock (`RtClass::HardRT`) **实现** `RtSafeCtrl`；Feetech (`RtClass::SerialRT`) **不实现**（不挂 rt_thread） |
| **C4** | `safety_stop_fast` 伪代码 `entries: Vec<(u8, i32)>` 在急停路径分配 → 内存压力下可能分配失败 | **§7.2 改预分配**：`FeetechAdapter` 加 `safety_stop_buf: Vec<(u8, i32)>` 字段；`connect()` 时 `with_capacity(joint_count)` + 一次性 push 占位；`safety_stop_fast` 调时只改值不重新分配 |

v1-v5 文档归档 git history。v6 是当前权威。

---

## §1 问题陈述（v4-v5 保留）

### 1.1 三类 adapter，三条 executor

| `RtClass` | 典型硬件 | RT 合规性 | Executor |
|---|---|---|---|
| `HardRT` | EtherCAT / RT-CAN / DMA mmap / mock_hard_rt | 可证明 bounded latency + no alloc + no block | `s0_rt_thread` (SCHED_FIFO) |
| `SerialRT` | 串口舵机 / USB-RS485 / Modbus RTU | 控制语义属 S0；IO ms 级 syscall | **新增** `s0_serial_io_thread` |
| `SoftRT` | RealSense / webcam / vendor SDK / 网络 sensor | 仅观测，不直接驱动 motor | `s0_io_thread[0..N]` (v0.4) |

判定基准：可证明的最坏延迟 + RT 铁律合规性。

### 1.2 双重缺口（v3 已识别）

A：state_machine 不驱动 adapter；B：`latest_cmd` 无门控（OutputPolicy 解决）。

### 1.3 Schema / state_machine 修订（pre-2 修补）

详 v3 §1.3。

### 1.4 v0.2 safety exit criteria 状态

`docs/10_verification.md:102` "kill -9 s0 → 硬件 fail-safe < 500 ms"
**task-30 完成 ≠ 此 SLA 通过**。详 §9。

---

## §2 设计原则（v5 16 条 + v6 加 3）

1-16：v5 全保留。

**17（v6 新加）**：**`safety_stop` 路径必须区分"下发"和"确认"**。下发成功
仅升级到 `StopCommanded`；只有读回 `Torque_Enable=0` 才升 `TorqueOffConfirmed`。
"假装已断扭"是不诚实的安全状态广播。

**18（v6 新加）**：**RT 路径所有控制动作必须可 atomic 编码**（u8/u16/u32/u64）。
任何 ArcSwap / Mutex / Channel send 都不得在 rt_thread 主循环内出现，连冷
路径也不行（rt_thread 不存在"冷路径"，主循环每个分支都是 hot）。复杂参数
请求（含 String / Vec / Box）→ `RobotError::Unsupported`，强制走 SerialRT 路径
或 v0.4 升级。

**19（v6 新加）**：**安全路径不分配**。Emergency / SafetyStop 的所有缓冲必须
在 `connect()` / 启动期预分配；运行期 OOM / 内存压力下急停路径**仍能执行**。

---

## §3 架构图（v6 = v5 + atomic 化 HardRT 控制路径）

```
┌─── robot-s0 ────────────────────────────────────────────────────────────┐
│                                                                         │
│  ipc_thread (state_machine)                                             │
│  ┌─────────────────────────────────────────────────┐                    │
│  │ executor_ctrl: Box<dyn ExecutorCtrlClient>      │                    │
│  │  ↓ on transition:                                │                    │
│  │  - set_policy(p)        — atomic store          │                    │
│  │  - safety_stop()        — fire-and-forget       │                    │
│  │  - request_normal(req)  — try_send / atomic     │                    │
│  │  - poll_last_normal_resp() — peek result         │                    │
│  └─────────────┬───────────────────────────────────┘                    │
│                │ trait                                                  │
│  ╔═════════════▼═══════════════════════════════════════════════════════╗│
│  ║                                                                     ║│
│  ║  HardRtCtrlClient impl       SerialIoCtrlClient impl                ║│
│  ║  ┌────────────────────────┐  ┌─────────────────────────────────┐    ║│
│  ║  │ policy: AtomicU8       │  │ policy: AtomicU8                │    ║│
│  ║  │ emergency_action: AU8  │← │ emergency_tx: ch (depth=8)      │    ║│
│  ║  │ normal_action:    AU8  │  │ estop_atomic: AtomicBool (兜底) │    ║│
│  ║  │ action_arg:       AU8  │  │ pending_normal: ArcSwap (普通)  │    ║│
│  ║  │ action_seq:       AU64 │  │ last_resp:      ArcSwap         │    ║│
│  ║  │ safety_latched:   AB   │  │                                 │    ║│
│  ║  │ (无 ArcSwap，无 Vec)   │  │                                 │    ║│
│  ║  └──────────┬─────────────┘  └────────────┬────────────────────┘    ║│
│  ║             │                              │                        ║│
│  ║             ▼                              ▼                        ║│
│  ║  ┌────────────────────────┐    ┌─────────────────────────────────┐  ║│
│  ║  │ rt_thread (SCHED_FIFO) │    │ serial_io_thread (普通)         │  ║│
│  ║  │ tick @ 50Hz：           │    │ select! {                        │  ║│
│  ║  │ 1. emergency_action    │    │   recv emergency_rx → exec       │  ║│
│  ║  │      .swap(NONE)       │    │   pending normal → handle_normal │  ║│
│  ║  │      → if !=NONE:      │    │   default(20ms) → tick:          │  ║│
│  ║  │        adapter.        │    │     check estop_atomic           │  ║│
│  ║  │          safety_stop_rt│    │     按 OutputPolicy gate         │  ║│
│  ║  │        latched=true    │    │     if StopCommanded:            │  ║│
│  ║  │ 2. normal_action       │    │       confirm_torque_off()       │  ║│
│  ║  │      .swap(NONE)       │    │       update safety_state        │  ║│
│  ║  │      → if !=NONE       │    │     adapter.write/read           │  ║│
│  ║  │        & !latched:     │    │ }                                │  ║│
│  ║  │        dispatch        │    │                                  │  ║│
│  ║  │ 3. policy gate         │    │  独占 SerialRT adapter           │  ║│
│  ║  │ 4. write_command       │    │  + OutputSafetyProbe             │  ║│
│  ║  │ 5. read_state          │    └──────────────────────────────────┘  ║│
│  ║  │ 6. publish snapshot    │                                          ║│
│  ║  │ 全程零 alloc / 零 Arc   │                                          ║│
│  ║  └────────────────────────┘                                          ║│
│  ╚═════════════════════════════════════════════════════════════════════╝│
│                                                                         │
│  HotShared.safety_state_slot (AtomicU8 编码 OutputSafetyState)         │
│                  ↑ written by both executors                           │
│  pub_thread reads it → state topic safety_state 字段（pre-2 schema）   │
└─────────────────────────────────────────────────────────────────────────┘
```

**v5 → v6 主要差异**：

- **HardRtCtrlClient 内部全 atomic** —— ArcSwap / Vec / Box 移到 v6 之外（不存在）
- emergency / normal action 拆两 atomic —— 优先级独立，互不覆盖
- `safety_latched: AtomicBool` —— EStop 触发后 rt_thread 不响应 normal enable
- `safety_state_slot` AtomicU8（编码 5 variant of `OutputSafetyState` 含新增的
  `StopCommanded`）—— 跨 executor 共享，pub_thread 读入 state topic
- serial_io_thread 加 `confirm_torque_off()` 步骤（StopCommanded → 下 tick 读回
  → 升级 / 降级 safety_state）

---

## §4 ExecutorCtrlClient trait（v5 保留 + v6 atomic 化）

### 4.1 trait 定义（v5 保留，4 方法）

```rust
pub trait ExecutorCtrlClient: Send + Sync {
    fn set_policy(&self, policy: OutputPolicy);
    fn safety_stop(&self);
    fn request_normal(&self, req: AdapterNormalReq) -> NormalReqStatus;
    fn poll_last_normal_resp(&self) -> Option<AdapterResp>;
}
```

### 4.2 HardRT 实现：v6 全 atomic（**移除所有 ArcSwap / Vec**）

```rust
/// HardRT executor 的 ctrl client。**所有字段都是 atomic** —— rt_thread 主
/// 循环零 alloc / 零 Arc refcount。
pub struct HardRtCtrlInner {
    /// `OutputPolicy` 编码（`#[repr(u8)]`，6 variant + 0=Disabled 兜底）
    policy: AtomicU8,

    /// **紧急 action**：rt_thread tick **优先**消费。非零值表示有紧急动作。
    /// 编码：1 = SafetyStop。
    /// 不被 normal_action 覆盖（拆开 channel 的精神）。
    emergency_action: AtomicU8,

    /// **普通 action**：rt_thread tick 在消费完 emergency 后处理。
    /// 编码：1=Connect / 2=Disconnect / 3=Enable(true) / 4=Enable(false) /
    ///        5=SetMode(Position) / 6=HoldCurrent
    /// 复杂请求（带参数）→ `request_normal` 返 `Unsupported`，不入此 slot。
    normal_action: AtomicU8,

    /// `normal_action` 的伴随参数（小标量；u8 足够编码 ControlMode 等）。
    /// rt_thread 在 `swap` 出 normal_action 后才读这个，无 race。
    action_arg: AtomicU8,

    /// **每次 set 加 1**；rt_thread 完成后 `last_completed_seq.store(seq)`。
    /// state_machine 据此判断"我上次的 request_normal 是否已被执行"。
    action_seq: AtomicU64,
    last_completed_seq: AtomicU64,
    last_completed_result: AtomicU8, // 0=Ok, 1=Err{InvalidParam}, 2=Err{HardwareFault}, ...

    /// **safety latched**：EStop / Fault 触发后 rt_thread 拒绝 normal Enable(true)
    /// 直到 state_machine 显式 ClearEStop（`safety_latched.store(false)`）。
    /// 防 v5 C1 bug：emergency 后随即 normal Enable(true) 把 SafetyStop 覆盖。
    safety_latched: AtomicBool,
}

pub struct HardRtCtrlClient {
    inner: Arc<HardRtCtrlInner>, // Arc**只在 spawn 时 clone 一次**，rt_thread 持
                                   // &Arc 生命周期 = 进程；运行期不 clone/drop
}

impl ExecutorCtrlClient for HardRtCtrlClient {
    fn set_policy(&self, policy: OutputPolicy) {
        self.inner.policy.store(policy as u8, Ordering::Release);
    }

    fn safety_stop(&self) {
        self.inner.emergency_action.store(EMERGENCY_SAFETY_STOP, Ordering::AcqRel);
        // 不 latch 这里 —— 由 rt_thread 完成后再 latch（保证"已开始执行"才锁）
    }

    fn request_normal(&self, req: AdapterNormalReq) -> NormalReqStatus {
        let (action_code, arg) = match encode_action(&req) {
            Some(pair) => pair,
            None => return NormalReqStatus::PreviousErr(RobotError::Unsupported(
                "HardRT path: complex AdapterNormalReq not encodable as atomic"
            )),
        };
        // safety latched 时拒绝 Enable(true)（防 C1）
        if action_code == NORMAL_ENABLE && arg == 1
            && self.inner.safety_latched.load(Ordering::Acquire)
        {
            return NormalReqStatus::PreviousErr(RobotError::Unsupported(
                "Enable(true) refused while safety latched (need ClearEStop first)"
            ));
        }
        self.inner.action_arg.store(arg, Ordering::Release);
        self.inner.normal_action.store(action_code, Ordering::Release);
        let seq = self.inner.action_seq.fetch_add(1, Ordering::AcqRel);
        NormalReqStatus::Submitted { seq }
    }

    fn poll_last_normal_resp(&self) -> Option<AdapterResp> {
        let last_seq = self.inner.last_completed_seq.load(Ordering::Acquire);
        let cur_seq = self.inner.action_seq.load(Ordering::Acquire);
        if last_seq < cur_seq {
            return None; // 还没执行
        }
        let code = self.inner.last_completed_result.load(Ordering::Acquire);
        Some(decode_resp(code))
    }
}
```

**rt_thread 主循环消费 atomic（v6 重写）**：

```rust
fn run_rt_loop<A: ManipulatorAdapter + RtSafeCtrl>(
    cfg: RtThreadConfig,
    mut adapter: A,
    ctrl: Arc<HardRtCtrlInner>,
    cmd_reader: ...,
    snapshot_pub: ...,
    safety_state_slot: Arc<AtomicU8>,
    ...
) {
    // pre-warmup：thread_local hazard pointer 等（task 26 已做）
    // ...
    loop {
        let deadline = ...;
        clock.sleep_until(deadline);
        rt_entry!({
            // 1. emergency_action 优先消费（永远先于 normal）
            let em = ctrl.emergency_action.swap(EMERGENCY_NONE, Ordering::AcqRel);
            if em == EMERGENCY_SAFETY_STOP {
                let result = adapter.safety_stop_rt();
                ctrl.safety_latched.store(true, Ordering::Release);
                let safety_state = match result {
                    Ok(()) => OUTPUT_SAFETY_STOP_COMMANDED,  // 不立刻 confirm
                    Err(_) => OUTPUT_SAFETY_STOP_FAILED,
                };
                safety_state_slot.store(safety_state, Ordering::Release);
                // HardRT 不做 confirm_torque_off —— 真 HardRT adapter 的
                // safety_stop_rt 通常是 mmap 写寄存器立即生效，bounded < 1µs，
                // 可以视为下发=确认。SerialRT 才需要 confirm 路径。
                // mock_hard_rt 直接进 TorqueOffConfirmed：
                if matches!(result, Ok(())) {
                    safety_state_slot.store(OUTPUT_SAFETY_TORQUE_OFF_CONFIRMED, Ordering::Release);
                }
            }

            // 2. normal_action（safety_latched 后 Enable(true) 已被 client 端拒）
            let na = ctrl.normal_action.swap(NORMAL_NONE, Ordering::AcqRel);
            if na != NORMAL_NONE {
                let arg = ctrl.action_arg.load(Ordering::Acquire);
                let result = match na {
                    NORMAL_CONNECT => adapter.connect(),
                    NORMAL_DISCONNECT => adapter.disconnect(),
                    NORMAL_ENABLE => adapter.enable_rt(arg == 1),
                    NORMAL_SET_MODE_POSITION => adapter.set_mode_rt(ControlMode::Position),
                    NORMAL_HOLD_CURRENT => adapter.hold_current_position(),
                    _ => Err(RobotError::Unsupported("rt_thread: unknown normal_action")),
                };
                let code = encode_resp(&result);
                ctrl.last_completed_result.store(code, Ordering::Release);
                let seq = ctrl.action_seq.load(Ordering::Acquire);
                ctrl.last_completed_seq.store(seq, Ordering::Release);
                if matches!(result, Ok(())) && na == NORMAL_ENABLE && arg == 1 {
                    ctrl.safety_latched.store(false, Ordering::Release);
                }
            }

            // 3. policy gate
            let policy_u8 = ctrl.policy.load(Ordering::Acquire);
            let policy = OutputPolicy::from_u8(policy_u8);
            if matches!(policy, OutputPolicy::ActiveForward)
                && !ctrl.safety_latched.load(Ordering::Acquire)
            {
                let cmd_guard = cmd_reader.load();
                let cmd = cmd_guard.as_view();
                let _ = adapter.write_command(&cmd);
            }

            // 4. read_state + publish
            let _ = adapter.read_state(&mut state_buf);
            snapshot_pub.publish(...);
        });
    }
}
```

**全程零 alloc / 零 Arc refcount**：所有读写都是原子标量；`Arc<HardRtCtrlInner>`
spawn 时 clone 一次，rt_thread 持只读引用，不再 clone/drop。`adapter` move 进
闭包独占。

### 4.3 SerialRT 实现（v5 保留）

`SerialIoCtrlClient` 用 emergency channel + AtomicBool 兜底 + ArcSwap pending
normal —— SerialRT executor 是普通线程，允许 ArcSwap。详 v5 §4.3。

---

## §5 OutputPolicy / OutputSafetyState / RtSafeCtrl（v6 修订）

### 5.1 OutputPolicy（C2 措辞修订）

```rust
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputPolicy {
    Disabled       = 0,
    ReadyHold      = 1,
    ActiveForward  = 2,
    HoldCurrent    = 3,
    ZeroCommand    = 4,
    StopTorque     = 5,
}

impl OutputPolicy {
    /// 接受 6 个有效值（0-5），其他 → `Disabled`（保守兜底，prevent UB
    /// from corrupted atomic）。
    #[must_use]
    pub const fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::ReadyHold,
            2 => Self::ActiveForward,
            3 => Self::HoldCurrent,
            4 => Self::ZeroCommand,
            5 => Self::StopTorque,
            _ => Self::Disabled, // 0 / 6+ 均归 Disabled（保守）
        }
    }
}
```

### 5.2 OutputPolicy 在两类 executor 的实现

详 v5 §5.2 表（不变）。

### 5.3 OutputSafetyState（v6 加 `StopCommanded`）

```rust
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputSafetyState {
    /// 启动期 / 断连后未恢复，硬件状态未知
    TorqueUnknown        = 0,
    /// 正常运行，torque on
    TorqueOn             = 1,
    /// **v6 新增**：safety_stop **已下发**（sync_write broadcast 已发送 OK），
    /// 但**未读回 Torque_Enable=0 确认**。下一 tick `confirm_torque_off`
    /// 走完才能升 `TorqueOffConfirmed`。
    /// HardRT 路径不会停留在此状态（mmap 写完即视为确认）；SerialRT 路径会。
    StopCommanded        = 2,
    /// safety_stop 完成 + 确认硬件已断扭（read Torque_Enable = 0 一致）
    TorqueOffConfirmed   = 3,
    /// safety_stop 调用返回 Err / 或 confirm 多次失败 → 硬件可能仍 torque on
    StopFailed           = 4,
}
```

序列化到 `safety_state_slot: Arc<AtomicU8>`，`from_u8` 同模式（0-4 valid，
其他 → `TorqueUnknown` 兜底）。

### 5.4 OutputSafetyProbe trait（v6 新增 - B2 fix）

```rust
/// SerialRT 执行 emergency safety_stop 后下 tick 调用，确认硬件真断扭。
///
/// **独立路径**，不污染 `ManipulatorAdapter::read_state` / `JointStateBuf`：
///
/// - read_state 是 hot path（每 tick 跑），加 Torque_Enable 字段会拖慢
///   sync_read packet（每个 motor 多读 1 byte）
/// - safety probe 是 cold path（仅 EStop / safety_stop 后用），偶发 RTT OK
/// - 类型隔离：观测（位置/速度）vs 安全状态（torque enable mask）
///
/// HardRT executor 不调本 trait（mmap 写 torque enable 是即时生效的）。
pub trait OutputSafetyProbe {
    /// 读所有 joint 当前 `Torque_Enable` 寄存器到 `out[..joint_count]`。
    /// `out.len()` 须 >= `joint_count`，否则返 `InvalidParameter`。
    fn read_torque_enable_mask(&mut self, out: &mut [bool]) -> Result<(), RobotError>;
}
```

`FeetechAdapter` 实现：sync_read `Torque_Enable` 寄存器（addr 40, 1 byte）
→ 解码到 `out`。预分配 buffer 同 §7.2。

`MockAdapter` 实现：返回内部 `enabled` 字段镜像。

### 5.5 RtSafeCtrl（v5 保留 + C3 修正实施方）

```rust
pub trait RtSafeCtrl {
    fn safety_stop_rt(&mut self) -> Result<(), RobotError>;
    fn enable_rt(&mut self, on: bool) -> Result<(), RobotError>;
    fn set_mode_rt(&mut self, mode: ControlMode) -> Result<(), RobotError>;
    // write_command(mode=Stop) 合约保留（v5 §5.3）
}
```

**C3 修正**：

- **`MockAdapter` 实现** `RtSafeCtrl`：mock_hard_rt 是 `RtClass::HardRT`，rt_thread
  会调；实现就是简单状态字段操作（无 IO 无 alloc，trivially RT-safe）+ 单测验
  `assert_no_alloc` scope 内不 panic
- **`FeetechAdapter` 不实现** `RtSafeCtrl`：feetech 是 `RtClass::SerialRT`，挂
  serial_io_thread；rt_thread 不调它。`spawn_rt_thread<A: ManipulatorAdapter +
  RtSafeCtrl>` trait bound 让编译期就拒 SerialRT adapter 走 HardRT 路径

---

## §6 SerialRT executor 的 confirm_torque_off 路径（v6 新增 - B1/B2 闭环）

```rust
fn tick_hardware(
    adapter: &mut dyn ManipulatorAdapter,
    probe: &mut dyn OutputSafetyProbe,    // v6 新增
    state: &mut IoState,
    policy: OutputPolicy,
    ctx: &SerialIoCtx,
) {
    // ... 已有 read_state / policy gate / mode 路由 / hold ... (v5 §5.6)

    // v6 新增：StopCommanded → 下 tick confirm
    if state.safety_state == OutputSafetyState::StopCommanded {
        // 用 adapter 内预分配 buffer（避免 alloc）
        let n = state.joint_count;
        let mask = &mut state.confirm_buf[..n];
        match probe.read_torque_enable_mask(mask) {
            Ok(()) => {
                if mask.iter().all(|&b| !b) {
                    // 全部 Torque_Enable=0 → 升级 confirmed
                    state.safety_state = OutputSafetyState::TorqueOffConfirmed;
                } else {
                    // 至少一个 motor 仍 torque on → 不一致
                    state.safety_state = OutputSafetyState::AdapterUncertain;
                    let _ = ctx.fault_tx.try_send(FaultCode::AdapterUncertain);
                }
            }
            Err(e) => {
                // 读失败：升级到 StopFailed（保守）
                tracing::error!(?e, "confirm_torque_off failed");
                state.safety_state = OutputSafetyState::StopFailed;
                let _ = ctx.fault_tx.try_send(FaultCode::SafetyStopFailed);
            }
        }
        ctx.safety_state_slot.store(state.safety_state as u8, Ordering::Release);
    }
}

fn handle_safety_stop(
    adapter: &mut dyn ManipulatorAdapter,
    state: &mut IoState,
    ctx: &SerialIoCtx,
) {
    let result = adapter.safety_stop(); // feetech impl 走 fast path
    match result {
        Ok(()) => {
            // **v6 修订（B1）**：不立刻 confirmed
            state.safety_state = OutputSafetyState::StopCommanded;
        }
        Err(e) => {
            tracing::error!(?e, "safety_stop FAILED — hardware state UNKNOWN");
            state.safety_state = OutputSafetyState::StopFailed;
            let _ = ctx.fault_tx.try_send(FaultCode::SafetyStopFailed);
        }
    }
    ctx.safety_state_slot.store(state.safety_state as u8, Ordering::Release);
}
```

`IoState` 加 `confirm_buf: Vec<bool>`（启动期 `with_capacity(MAX_JOINTS)` +
`resize`），运行期 `&mut [..n]` 切片复用 —— **零运行期 alloc**。

---

## §7 Feetech `safety_stop_fast()` 设计（v5 保留 + C4 修正预分配）

### 7.1 当前状态 + 时延对比（v5 保留）

`enable(false)` 13 motor × 3 retry × 150ms = ~5.85s 最坏 → emergency 不可用。

### 7.2 v6 预分配实现（C4 fix）

```rust
pub struct FeetechAdapter<T: SerialTransport> {
    // ... 现有字段 ...

    /// **v6 新增**：emergency safety_stop 路径预分配 buffer。
    /// `connect()` 时按 joint_count `with_capacity` + 一次性 push 占位
    /// (id, 0)；`safety_stop_fast` 调时只更新元素值，不重新 alloc。
    /// 防 §2.19 "安全路径不分配"。
    safety_stop_buf: Vec<(u8, i32)>,

    /// **v6 新增**：confirm_torque_off 用的 Torque_Enable raw 缓冲。
    confirm_torque_buf: Vec<i32>,
}

impl<T: SerialTransport> FeetechAdapter<T> {
    pub fn safety_stop_fast(&mut self) -> Result<(), FeetechError> {
        // 复用预分配 buffer，运行期不 alloc
        for entry in self.safety_stop_buf.iter_mut() {
            entry.1 = 0; // 全部置 Torque_Enable = 0
        }
        // sync_write broadcast (id=0xFE)；不等 ACK；1 次串口往返 ~1-2ms
        self.bus.sync_write_by_id("Torque_Enable", &self.safety_stop_buf)?;
        Ok(())
    }
}

impl<T: SerialTransport> ManipulatorAdapter for FeetechAdapter<T> {
    fn safety_stop(&mut self) -> Result<(), RobotError> {
        // 默认 trait impl 走 enable(false) 慢路径；feetech override 走 fast
        self.safety_stop_fast().map_err(RobotError::from)
    }
    fn connect(&mut self) -> Result<(), RobotError> {
        // ... 现有 handshake / joint_id 缓存 ...

        // v6：safety buffer 预分配
        self.safety_stop_buf.clear();
        for &id in &self.joint_ids {
            self.safety_stop_buf.push((id, 0));
        }
        self.confirm_torque_buf.resize(self.joint_ids.len(), 0);
        Ok(())
    }
}

impl<T: SerialTransport> OutputSafetyProbe for FeetechAdapter<T> {
    fn read_torque_enable_mask(&mut self, out: &mut [bool]) -> Result<(), RobotError> {
        let n = self.joint_ids.len();
        if out.len() < n {
            return Err(RobotError::InvalidParameter("out buffer too small"));
        }
        // 预分配 confirm_torque_buf；sync_read by-id 走 by-id hot path
        self.bus.sync_read_by_id_into(
            "Torque_Enable",
            &self.joint_ids,
            &mut self.confirm_torque_buf,
        )?;
        for (i, &raw) in self.confirm_torque_buf[..n].iter().enumerate() {
            out[i] = raw != 0; // 1 = enabled, 0 = disabled
        }
        Ok(())
    }
}
```

---

## §8 OutputSafetyState + state_machine（v3 保留）

详 v3 §6 + pre-2 transition 表加 `* → Fault(SafetyStopFailed)`。

---

## §9 SIGTERM graceful + SIGKILL blocking gap（v5 保留）

### 9.1 SIGTERM graceful

详 v3 §7。

### 9.2 SIGKILL：明确 task-30 ≠ v0.2 SLA

详 v5 §9.2。task-30 完成只完成"graceful 软件断扭" + "EStop 真断扭" + "
StopCommanded → confirmed" 路径；硬件 fail-safe 留 task-30-post。

---

## §10 测试覆盖（v5 + v6 新增）

继承 v5 全部 + v6 加：

| 用例 | 验证 |
|---|---|
| `HardRtCtrlInner` 全字段 atomic（无 ArcSwap / 无 Vec）；`#[derive]` 不含 alloc 字段 | B3 |
| rt_thread 主循环 `assert_no_alloc` scope 全过（含 emergency / normal 处理）| B3 + R11 |
| `safety_stop()` 后 `Enable(true)` `request_normal` 返 `Unsupported`（safety_latched） | C1 |
| safety_stop 下发成功 → `safety_state == StopCommanded`（**不**是 TorqueOffConfirmed）| B1 |
| 下一 tick `confirm_torque_off` 读回 mask 全 0 → 升 `TorqueOffConfirmed` | B1 + B2 |
| 下一 tick read 失败 → `StopFailed` + `FaultCode::SafetyStopFailed` 上报 | B2 |
| `confirm_torque_off` mask 部分 1 → `AdapterUncertain` + Fault | B2 |
| `OutputSafetyProbe::read_torque_enable_mask`（feetech mock transport）解码 sync_read response 正确 | B2 |
| `safety_stop_fast` 调用前 `connect()` 必先 `safety_stop_buf` 预分配；运行期 `safety_stop_fast` 在 `assert_no_alloc` scope 不 panic | C4 + R11 |
| `OutputPolicy::from_u8` 0/6/255 → `Disabled` 兜底 | C2 |
| `MockAdapter` 实现 `RtSafeCtrl`（spawn_rt_thread 编译通过）；`FeetechAdapter` 不实现（spawn_rt_thread 编译期拒）| C3 |
| **【blocking gap】** SIGKILL → torque off 不通过（标 known issue） | 不假装 |

---

## §11 迁移计划（v5 重排 + v6 加 OutputSafetyProbe）

### 11.1 前置 task-30-pre（v3-v5 沿用）

详 v5 §11.1。

### 11.2 主体 M1-M7（v6 拆 OutputSafetyProbe）

| # | 主题 | 工作量 | 依赖 |
|---|---|---|---|
| **M1** | `ExecutorCtrlClient` trait + `OutputPolicy(AtomicU8)` + `OutputSafetyState(含 StopCommanded)` + `AdapterNormalReq` + atomic 编码常量；纯类型 + 单测 | 1 h | pre-3a/b |
| **M2** | `RtSafeCtrl` 子 trait + `MockAdapter` 实现 + `assert_no_alloc` 单测；`ManipulatorAdapter::write_command(mode=Stop)` 合约 doc | 1 h | M1 |
| **M3** | **v6 新增**：`OutputSafetyProbe` trait + `FeetechAdapter` 实现 + `MockAdapter` 实现；mock transport 单测 | 1 h | pre-1 |
| **M4** | `FeetechAdapter::safety_stop_fast()` + `safety_stop` override + 预分配 buffer；连接时 `safety_stop_buf` / `confirm_torque_buf` resize；mock transport 单测 broadcast wire bytes + 时延 | 1 h | M3 |
| **M5** | `HardRtCtrlClient` 实现（**全 atomic**）+ rt_thread 主循环消费 emergency / normal action + safety_latched 拒绝 enable + assert_no_alloc soak | 1.5 h | M1 / M2 |
| **M6** | `SerialIoCtrlClient` + `serial_io_thread` skeleton + `select!` loop + `tick_hardware` 含 `confirm_torque_off` 路径；feature flag off | 2.5 h | M1 / M3 / M4 / pre-3b |
| **M7** | `s0_daemon` dispatch by `RtClass`；ipc_thread 加 `Box<dyn ExecutorCtrlClient>` + transition hook 真驱动；删 legacy bootstrap；feature flag default on；smoke 通过下个 release 删 flag | 1.5 h | M5 / M6 / pre-2 |

**总 ~9.5 h**（v5 估 8 h；v6 加 OutputSafetyProbe + safety_state 二阶段确认
+ 全 atomic HardRT 重写 多 1.5 h，更真实）。

---

## §12 风险（v3-v5 R1-R12 保留 + v6 加）

### v6 R13：HardRT 路径"原子编码不下"的请求

**风险**：未来某 HardRT adapter 的 `set_mode(Velocity)` 需要带速度上限参数（e.g.
`SetMode { mode: Velocity, max_rpm: u32 }`）→ 不可单字节 atomic 编码 → request_
normal 返 `Unsupported`，需要 v0.4 升级（多 atomic 字段 / 跨 tick 多步 commit
等）。

**缓解**：v0.2 范围内 HardRT 控制集小（Connect / Disconnect / Enable bool /
SetMode(Position only) / HoldCurrent），全部 atomic 编码够用；v0.3 加复杂参数
时再扩。

### v6 R14：SerialRT confirm_torque_off 增加一次 RTT

**风险**：`StopCommanded → 下 tick read_torque_enable_mask` 增加一次 sync_read
RTT（feetech ~5ms 13 motor）；emergency 路径总时延 = safety_stop_fast(~2ms)
+ confirm(~5ms) ≈ 7ms。仍远低于 verification SLA 250 ms。

**缓解**：可接受；测试覆盖确认时延 < 50ms 即过。

### 回滚（v5 沿用 + 加 OutputSafetyProbe revert）

每 M commit 独立 revertable；OutputSafetyProbe 回退 → safety_state 直接停留
`StopCommanded`（**不**升 `TorqueOffConfirmed`）—— 安全语义降级（保守）但可用。

---

## §13 v6 决议请求

请 review 并回复：

1. v6 整体（HardRT 全 atomic + emergency/normal 拆 + safety_latched +
   StopCommanded 二阶段 + OutputSafetyProbe + safety_stop_fast 预分配）approve？
2. **`OutputSafetyState` 加 `StopCommanded` variant** 是否同意（B1 fix）？
3. **`OutputSafetyProbe` 独立 trait** 而非塞进 `read_state` 是否同意（B2 fix）？
4. **HardRT 全 atomic（移除所有 ArcSwap）** + 复杂请求 `Unsupported` 是否接受（B3 fix）？
5. **`safety_latched` AtomicBool** + Enable(true) reject 直到 ClearEStop 是否接受（C1 fix）？
6. M5 / M6 / M7 拆开（v5 M5+M6 → v6 M5/M6/M7）总 9.5h 是否合理？
7. M7 行为切换点是否拆两 commit（"flag on" + "删 legacy"）？
8. 真机最低门槛 M7 smoke：v5 8 项 + v6 新增：
   - `mock_s1` 发 `Stop` → `safety_state` 先 `StopCommanded` 后 `TorqueOff
     Confirmed`（确认 confirm_torque_off 路径）
   - `feetech_poke read Torque_Enable` 在 SIGTERM 后必须 = 0（emergency 路径
     可工作）
   - 同意 / 加项？

review 通过我按 pre-1 → pre-2 → pre-3 → M1-M7 顺序动手 task-30 实施。
