# Task-30 设计方案：state_machine ↔ adapter 执行链路

> **状态**：**v4**，等再次 review
> **历史**：v1（commit `4cca734`，523 行草案）→ v2（commit `130b44b`，718 行）
> → v3（commit `f7bdc00`，744 行）→ v4（本版本，user v3 二轮 review 后再修订）
> **作者**：Claude Code
>
> **v4 关键修订**：v3 默认所有 adapter 都走单一 `adapter_io_thread`（实际把
> `rt_thread` 永久降级为数据通路）—— 这是 **过度修订**。原设计 §1.2 / §1.4
> "HardRT adapter 直挂 `s0_rt_thread`" 对**真 HardRT adapter**（EtherCAT /
> RT-CAN / DMA mmap，可证明 < 100 µs RTT + 不 alloc + 不 block）**仍然成立**。
> Feetech 的问题不是设计错，是 **adapter 分类错**（标 HardRT 但 ms 级串口 IO）。
>
> v4 解法：保留原设计 HardRT 路径不动；**新增第三类 `RtClass::SerialRT`** 与
> HardRT / SoftRT 并列；`s0_daemon` 按 `adapter.info().rt_class` **dispatch
> executor**。Feetech 走 SerialRT executor，未来 EtherCAT 走 HardRT executor。
>
> 关键不是协议名字，而是**可证明的最坏延迟 + RT 铁律合规性**。

---

## §0 v3 → v4 修订记录

User 在 v3 review 后给出**架构级方向修正**（非具体 bug）：

> "保留原设计不动：HardRT adapter 仍然由 rt_thread 直接执行。新增一条明确的
> SerialRT control path，专门给 Feetech 这种'控制级硬件，但串口 IO 不满足
> RT 铁律'的 adapter 用。"

v4 全部接受：

| v3 设计选择 | v4 修订 |
|---|---|
| 单一 `adapter_io_thread` 接管所有 adapter；rt_thread 永久降级为读 HotShared 的数据通路 | **三类 executor 并列**：`spawn_rt_thread`（HardRT，原设计不动）+ **新增** `spawn_serial_io_thread`（SerialRT）+ `spawn_io_thread`（SoftRT，已有）；`s0_daemon` 按 `adapter.info().rt_class` dispatch |
| `rt_thread` 改读 `HotShared.cmd_slot`，写 `HotShared.state_slot`；不再直接调 adapter | **rt_thread 不改**：保留 `read command slot → adapter.write_command/read_state → write rt_snapshot` 原设计；只用于真 HardRT adapter |
| 默认所有 adapter 都走 io_thread 路径 | **按 adapter class 路由**；同进程可同时跑 HardRT + SerialRT + SoftRT 任意组合 |
| OutputPolicy / emergency channel / IoState 在共享 adapter_io_thread | **OutputPolicy / emergency / IoState 仅 SerialRT executor 用**；HardRT executor 保留原 rt_thread 行为；SoftRT 不需要（sensor 无 motor 输出） |
| pre-1 把 Feetech 重分类 `RtClass::SerialRT`（v3 已规划） | 保留 + 升级：**`RtClass::SerialRT` 是设计 §4.6 的扩展**（新 enum variant），不是用现有 SoftRT 凑合；走 CHANGELOG + dimos 同步 |
| v3 §3 架构图：单一 adapter_io_thread | **v4 §3 架构图重画**：rt_thread + serial_io_thread + io_thread 三类并列；executor pool 概念 |
| v3 §6 迁移计划 M1-M5 默认 Feetech-only | **v4 §6 迁移计划重排**：M1 加 SerialRT enum；M2 OutputPolicy；M3 spawn_serial_io_thread skeleton；M4 接 state_machine hook；**M5 dispatch by rt_class**；现有 mock_hard_rt 仍走 rt_thread 路径不变 |

v3 的 7 主问题 + 3 次问题修订**全部继承到 v4**（只是受影响范围从"adapter_io_
thread" 改成 "serial_io_thread"，逻辑不变）。

v1 / v2 / v3 文档归档 git history（commits `4cca734` / `130b44b` / `f7bdc00`）。
v4 是当前权威。

---

## §1 问题陈述（v4 重述）

### 1.1 核心识别

**原设计 §1.2 / §1.4 / §6.7 没错** —— `s0_rt_thread` 直接调 HardRT adapter
是合理的：

- 真 HardRT adapter（EtherCAT / RT-CAN / DMA mmap）可证明 < 100 µs RTT
- rt_thread 在 SCHED_FIFO + mlockall + CPU pin 下满足 RT 铁律
- ipc_thread 写 latest-command slot → rt_thread 读 → 调 adapter → 写 rt_snapshot：
  最短延迟，最低 jitter

**问题在 adapter 分类错**：

- `crates/robot-adapters/feetech/src/adapter.rs:202` 当前标 `RtClass::HardRT`
- 实测 13-motor sync_read 5 ms / sync_write 1 ms（task 27c flamegraph baseline）
- 串口 read/write 有系统调用 + 不可控阻塞 + 内核驱动 wait queue + USB 协议层
  RTT
- timeout / 重试 / sleep 都不能进 rt_thread
- **从架构开始 8 个月一直在违约 RT 铁律**，task 28 commit `940c563` 注释里早
  就承认是"目标分类"

### 1.2 三类 adapter，三条 executor

| `RtClass` | 典型硬件 | RT 合规性 | Executor | 状态 |
|---|---|---|---|---|
| **`HardRT`** | EtherCAT / RT-CAN / DMA mmap / 自家 FPGA / shared-mem motor controller | 可证明 bounded latency + no alloc + no block + no log + no syscall（除 ioctl） | `s0_rt_thread`（SCHED_FIFO） | **原设计**（v0.2 mock_hard_rt 已用） |
| **`SerialRT`** | 串口舵机 / USB-RS485 / Modbus RTU / 普通 USB-CDC 控制器 | 控制语义属 S0（直接产生运动输出），但 IO 不满足 RT 铁律（ms 级 RTT，syscall，timeout） | **新增** `s0_serial_io_thread`（普通优先级 + RobotClock + OutputPolicy gate + emergency channel） | **task-30 v4 新增** |
| **`SoftRT`** | RealSense / webcam / vendor C++ SDK / 网络传感器 | 仅产生观测，不直接驱动 motor | `s0_io_thread[0..N]`（普通优先级 poll） | **原设计**（v0.4 真用） |

**关键**：分类靠"可证明的 RT 合规性 + 是否产生运动输出"，不靠协议名字。
EtherCAT 用了会阻塞的用户态库 / 动态分配 / 内部 mutex 也不能标 HardRT；某些
CAN 驱动 bounded + no alloc 就可以是 HardRT。

### 1.3 双重缺口（v2/v3 已识别，v4 保留）

#### A：state machine 不驱动 adapter

`EStop / Fault / Safe / Stop / CtrlLostHold / cmd.mode` 全部不闭环。详 v3 §1.2.A
（不复制）。

#### B：`latest_cmd` 无门控

即便缺口 A 修了，io 层无条件转发 `latest_cmd` 会冲掉 hold / Safe / Fault
语义。**`OutputPolicy` 解决**（详 §4）。

### 1.4 Schema 已欠债（v3 已识别，v4 保留）

`docs/00_design.md` §3.4 要求 state topic 报 `mode/control_link/output_mode`，
但 `joint.fbs:36` 只有位置 / 速度 / 力矩字段；`pub_thread:237` 也没用 state_rdr
发字段。task-30 加 `safety_state` 时绕不开这个欠债。详 v3 §1.3。

### 1.5 SIGKILL fail-safe 是 blocking gap

软件做不到，留 task-30-post（servo timeout 标定 + 硬件继电器）。详 v3 §7.2。

---

## §2 设计原则（v4 = v3 + 一条新硬约束）

1-13. v3 已有 13 条全保留（RT 铁律 / systemd / trait 签名 / panic abort /
向后兼容 / 安全链路不长期 feature gated / adapter timeout / safety_stop 失败
路径 / 不阻塞 ipc / emergency 不丢 / SIGKILL blocking gap / schema 修订走
schema_compat / RobotClock 替 Instant）。

**14（v4 新加）**：**按 `RtClass` dispatch executor**。同进程可同时跑
`HardRT + SerialRT + SoftRT` 任意组合；executor 之间只通过 `LatestCommandSlot`
+ `RtSnapshot` + `OutputPolicy` + emergency channel 几个标准基础设施交互；
**不**让某一类 executor 替代另一类。

---

## §3 架构图（v4 重画）

### 3.1 当前（v0.2 task 29 之后，**所有 adapter 都挂 rt_thread**）

```
┌─── robot-s0 ─────────────────────────────────────────────────┐
│                                                              │
│  ipc_thread (state_machine 反映器，不驱动 adapter)            │
│      │ writes latest-command slot                            │
│      ▼                                                       │
│  rt_thread (SCHED_FIFO)                                      │
│      adapter (move 进闭包)                                    │
│       ├─ MockAdapter (HardRT，OK)                             │
│       └─ FeetechAdapter (标 HardRT 但实际 ms 级，违约)         │
└──────────────────────────────────────────────────────────────┘
```

### 3.2 v4 后

```
┌─── robot-s0 ─────────────────────────────────────────────────────────┐
│                                                                      │
│  ipc_thread                                                          │
│  ┌────────────────────────────────────────────────┐                  │
│  │ state_machine (转换 + Guards)                   │                  │
│  │  ↓ on transition:                              │                  │
│  │ AdapterCtrlClient (非阻塞 facade):              │                  │
│  │   - set_policy(OutputPolicy) → arc-swap        │                  │
│  │   - safety_stop() → emergency tx + atomic       │                  │
│  │   - request_normal(req) → pending op slot       │                  │
│  └─────┬────────────────────┬─────────────────────┘                  │
│        │ 共享基础设施         │                                       │
│        │ HotShared (latest_cmd / rt_snapshot / safety_state /         │
│        │           output_policy)                                     │
│        ▼                    ▼                                        │
│  ┌──────────────────┐ ┌──────────────────────┐ ┌──────────────────┐ │
│  │ rt_thread        │ │ serial_io_thread     │ │ io_thread[0..N]  │ │
│  │ (SCHED_FIFO)     │ │ (普通优先级)          │ │ (普通优先级)      │ │
│  │ HardRT executor  │ │ SerialRT executor    │ │ SoftRT executor  │ │
│  │                  │ │                      │ │                  │ │
│  │ tick @ 50Hz:     │ │ select! {            │ │ poll sensor:     │ │
│  │   read cmd slot  │ │   recv emergency_rx  │ │   read camera /  │ │
│  │   按 OutputPolicy│ │     → handle now     │ │   vendor SDK     │ │
│  │   gate           │ │   recv normal_rx     │ │   write softrt_  │ │
│  │   adapter.write  │ │   default(20ms)→tick │ │     snapshot     │ │
│  │   adapter.read   │ │     按 OutputPolicy  │ │                  │ │
│  │   write snapshot │ │     gate; mode 路由   │ │                  │ │
│  │                  │ │   adapter.write/read │ │                  │ │
│  │                  │ │   有界 timeout       │ │                  │ │
│  │  独占 HardRT     │ │  独占 SerialRT       │ │  独占 SoftRT     │ │
│  │  adapter         │ │  adapter             │ │  adapter         │ │
│  └──────────────────┘ └──────────────────────┘ └──────────────────┘ │
│                                                                      │
│  s0_daemon main:                                                     │
│    for hw in config.hardware:                                        │
│      adapter = factory.build(hw)                                     │
│      match adapter.info().rt_class {                                 │
│        HardRT   => spawn_rt_thread(adapter, ...)                     │
│        SerialRT => spawn_serial_io_thread(adapter, ...)              │
│        SoftRT   => spawn_io_thread(adapter, ...)                     │
│      }                                                               │
│                                                                      │
│  pub_thread / watchdog / health 不变                                 │
└──────────────────────────────────────────────────────────────────────┘
```

**核心变化（vs v3）**：

- `rt_thread` 行为**不变**（保留 v0.2 task 23 的实现），只接 `RtClass::HardRT`
  adapter。原设计完全保留。
- `serial_io_thread` 是**新增**线程类型，与 rt_thread 并列，专给 SerialRT
  adapter。v3 的 OutputPolicy / emergency channel / IoState / RobotClock /
  timeout 等保护**都放这里**。
- `s0_daemon::main` 启动期按 `adapter.info().rt_class` **dispatch**：HardRT
  → 现有 `spawn_rt_thread`；SerialRT → 新 `spawn_serial_io_thread`；SoftRT →
  v0.4 加 `spawn_io_thread`（本 task 不实施）。
- 现有 `mock_hard_rt` adapter 保持 `RtClass::HardRT` 不动，走 rt_thread 路径，
  v0.2 测试 / smoke 行为零回归。
- Feetech adapter 改 `RtClass::SerialRT`（pre-1），走 serial_io_thread 路径。

---

## §4 OutputPolicy（v2/v3 保留，**仅 SerialRT executor 使用**）

7 variant 不变：`Disabled / ReadyHold / ActiveForward / HoldCurrent /
ZeroCommand / StopTorque`。`S0State → OutputPolicy` 映射表保留。详 v3 §4。

**v4 范围限定**：HardRT executor（rt_thread）的"按 OutputPolicy gate" 行为
**也保留**（rt_thread 主循环开头读 atomic policy slot，分支决定是否调
`adapter.write_command`），但**实现极简**：rt_thread 只读 atomic + 简单 match，
**不**做 emergency channel / IoState / 重试等复杂语义（那些是 SerialRT 的事）。

HardRT 路径 OutputPolicy 简化版：

```rust
// rt_thread 主循环（伪代码）
let policy = ctx.policy_slot.load(); // atomic load，几 ns
match *policy {
    OutputPolicy::ActiveForward => {
        let cmd = ctx.cmd_slot.load();
        let _ = adapter.write_command(&cmd.as_view()); // HardRT，<100µs
    }
    OutputPolicy::Disabled
    | OutputPolicy::StopTorque
    | OutputPolicy::HoldCurrent
    | OutputPolicy::ReadyHold
    | OutputPolicy::ZeroCommand => {
        // 不写 cmd；硬件 hold 由 adapter 内部 / 上层 disable 实现
    }
}
let _ = adapter.read_state(&mut state_buf);
ctx.snapshot_pub.publish(...);
```

复杂的语义（hold_current_position fallback safety_stop / mode change /
ControlMode::Stop 触发 emergency / safety_state 上报）**只在 SerialRT executor
里实现**。HardRT adapter 自己保证这些（write_command 在 OutputPolicy::Disabled
下不被调用就够了）。

---

## §5 SerialRT executor 详细设计（v4 = v3 §5 简化）

继承 v3 §5 全部内容（`AdapterReq` / `AdapterCtrlClient` 非阻塞 / `IoState` /
`select!` server loop / emergency 立即 dispatch / `tick_hardware` 按 policy
gate / Velocity Unsupported Fault / `safety_stop` 失败上报）。

**v4 简化点**：实现路径只在 SerialRT executor，不再涉及 rt_thread 改造。

### 5.1 模块布局

```
crates/robot-s0/src/
  serial_io_thread/
    mod.rs                 (pub use)
    config.rs              (SerialIoConfig: tick_period / timeout 等)
    handle.rs              (SerialIoThreadHandle: stop / metrics)
    server.rs              (run_serial_io: select loop)
    adapter_ctrl.rs        (AdapterCtrlClient + AdapterReq/Resp)
    io_state.rs            (IoState + OutputPolicy 联动)
```

### 5.2 入口签名

```rust
pub fn spawn_serial_io_thread<A>(
    cfg: SerialIoConfig,
    clock: Arc<dyn RobotClock>,
    adapter: A,
    cmd_reader: LatestCommandReader<JointCommandBuf>,
    snapshot_pub: RtSnapshotPublisher<RtSnapshot>,
    ctrl_client_init: AdapterCtrlClientInit, // policy / emergency / pending op
    fault_tx: crossbeam_channel::Sender<FaultCode>,
) -> io::Result<SerialIoThreadHandle>
where
    A: ManipulatorAdapter + Send + 'static,
```

签名与 `spawn_rt_thread` 对称（`cmd_reader` / `snapshot_pub` 共享类型 → ipc /
pub_thread / robotctl 等都能复用），便于 dispatch 时不感知。

### 5.3 OutputPolicy gate（详细版）

详 v3 §5.6 `tick_hardware`，照搬。新增：

- `safety_state` 通过 atomic / arc-swap 写到 `HotShared.safety_state_slot`，
  pub_thread 读出转发 state topic（依赖 pre-2 schema 修订）

### 5.4 emergency / normal channel + atomic estop（继承 v3 §5.1 / §5.2）

继承不变。

---

## §6 `s0_daemon` dispatch by RtClass（v4 新增章节）

启动期 `s0_daemon::run()` 拿到每个 `[[hardware]]` 段构造的 `Box<dyn Manipulator
Adapter>` 后：

```rust
fn spawn_executor_for_adapter(
    adapter: Box<dyn ManipulatorAdapter + Send>,
    common_ctx: ExecutorCommonCtx,
) -> Result<ExecutorHandle, RobotError> {
    match adapter.info().rt_class {
        RtClass::HardRT => {
            spawn_rt_thread(common_ctx.into_rt_cfg(), adapter, ...).map(ExecutorHandle::Rt)
        }
        RtClass::SerialRT => {
            spawn_serial_io_thread(common_ctx.into_serial_cfg(), adapter, ...).map(ExecutorHandle::Serial)
        }
        RtClass::SoftRT => {
            // v0.4 真用，本 task 暂不实施
            Err(RobotError::Unsupported("SoftRT executor in s0_daemon"))
        }
    }
}

enum ExecutorHandle {
    Rt(RtThreadHandle),
    Serial(SerialIoThreadHandle),
}

impl ExecutorHandle {
    fn request_stop(&self) { /* 转发 */ }
    fn join(self) -> thread::Result<()> { /* 转发 */ }
}
```

`graceful_stop` 接 `ExecutorHandle` 列表（v0.2 单 hardware → 一个 handle；
v0.4 多 hardware → vec），逐个 stop + join。

---

## §7 SIGTERM graceful + SIGKILL blocking gap（v3 保留）

详 v3 §7。SerialRT executor 的 shutdown 走 emergency channel + bounded timeout
+ adapter.disconnect。SIGKILL 仍是 blocking gap，留 task-30-post。

---

## §8 测试覆盖（v3 + v4 增）

继承 v3 全部 + 新增：

| 用例 | 验证 |
|---|---|
| `spawn_executor_for_adapter` 按 `RtClass::HardRT` 走 rt_thread 路径（mock_hard_rt 不回归）| dispatch 正确 |
| `spawn_executor_for_adapter` 按 `RtClass::SerialRT` 走 serial_io_thread 路径 | 新路径 |
| `RtClass::SoftRT` 在 v0.2 范围内返 `Unsupported`（不 panic 不忽略）| dispatch 严格 |
| 同时跑 mock_hard_rt（HardRT，rt_thread）+ feetech（SerialRT，serial_io_thread）→ 两条独立 executor 工作 | 多 executor 并存 |
| OutputPolicy 在 rt_thread 简化版正确门控（`Disabled` 不调 write_command）| HardRT 也 gate |
| OutputPolicy 在 serial_io_thread 完整语义（含 hold fallback / mode change / Stop emergency / safety_state 上报）| SerialRT 完整 |

---

## §9 迁移计划（v4 重排）

### 9.1 前置 task-30-pre（v3 沿用）

| # | 主题 | 工作量 | 评审级别 |
|---|---|---|---|
| **pre-1** | `RtClass::SerialRT` 加 enum variant；feetech adapter 改 `HardRT → SerialRT`；`docs/00_design.md` §4.6 设计修订评审 + dimos 同步；`docs/10_verification.md` 加 SerialRT baseline 行 | 30 min + 评审 | **§4.6 设计修订** |
| **pre-2** | state schema 补 `mode/control_link/output_mode/safety_state` + `Unknown` variant 兼容旧 writer + build_helpers + pub_thread 真发字段 + robotctl 展示 + schema_compat 测试 + state_machine 加 `* → Fault(SafetyStopFailed)` | 1.5 h + 评审 | **§3.4 / §1.4 设计修订** |
| **pre-3a** | `RobotError::Unsupported(&'static str)` variant + thiserror Display | 15 min | 纯增量 |
| **pre-3b** | （v3 的"adapter_io_thread 走 RobotClock"）→ v4 改成 **serial_io_thread 走 RobotClock**；rt_thread 不动（已经走 RobotClock） | 30 min | 纯增量 |

### 9.2 主体 M1-M5（v4 重排）

| # | 主题 | 工作量 | 依赖 |
|---|---|---|---|
| **M1** | `serial_io_thread` module skeleton：req / resp / client（非阻塞）/ server / OutputPolicy（共享类型） / IoState / SerialIoConfig / SerialIoThreadHandle；mock 单测 | 1.5 h | pre-3a/b |
| **M2** | `OutputPolicy` 落地：rt_thread 简化版 gate + serial_io_thread 完整 gate + `tick_hardware` 单测 | 1 h | M1 |
| **M3** | `hold_current_position` 默认 `Err(Unsupported)`；mock 显式 no-op；feetech override；trait 单测 | 30 min | pre-3a |
| **M4** | feature flag 启用：`spawn_serial_io_thread` 上线；`s0_daemon` 加 dispatch by `RtClass` 路径；HardRT 仍走原 rt_thread；保留 legacy bootstrap 防回归 | 2 h | pre-1 / pre-2 / M1-M3 |
| **M5** | ipc_thread transition hook 真驱动 adapter；删 legacy bootstrap；`ControlMode::Stop` 真断扭；Velocity → `Unsupported` Fault；feature flag 默认 on；smoke 通过后下个 release 删 flag | 1.5 h | M4 |

**总 ~6.75 h**（v3 估同；v4 复用 v3 的 SerialRT 实现，只是不动 rt_thread —
工作量平移）。

**feature flag 时间线**：M1-M3 不存在 / M4 默认 off / M5 真机 smoke 通过后 on /
**下个 release 必须 on + 删 flag**（安全链路不长期 feature gated）。

---

## §10 风险 + 回滚（v3 R1-R8 保留 + v4 加）

### v4 R9：HardRT executor 路径下 OutputPolicy 简化版的语义边界

**风险**：HardRT executor 的 OutputPolicy gate 是简化版（只是不调 write_command），
没有 hold_current_position / mode change / safety_stop fallback。如果未来某个
`RtClass::HardRT` adapter（比如 EtherCAT）想要 `CtrlLostHold` 也调 hold helper，
就需要再扩 rt_thread。

**缓解**：v0.2 真 HardRT adapter 只有 `mock_hard_rt`（不需要这些语义）；真上
EtherCAT 在 v0.4，那时再评估是否把"hold fallback"等语义复制到 rt_thread。简
化版到时再升级。**v4 不做**。

### v4 R10：dispatch by RtClass 引入运行时分支

**风险**：`s0_daemon` 启动期分支选 executor 类型，编译期不能完全静态化（adapter
是 trait object）。错配（adapter 自报 SoftRT 但实际有 motor 输出）→ 安全语义
丢失。

**缓解**：
- adapter 实现方写 `info()` 时按"可证明 RT 合规性 + 是否产生运动输出"严格
  分类（`docs/logging.md` 加 RtClass 对照说明）
- CI 加 `cargo deny`-style lint：检查每个 adapter crate 的 `info().rt_class`
  是否与 README / CHANGELOG 一致（人工 review）
- v0.4 真用 SoftRT executor 时再加运行时校验

### 回滚

- M5 revert → ipc_thread 不接 hook，回到 legacy bootstrap（rt_thread 仍跑）
- M4 revert → spawn_serial_io_thread 不上线；feetech 仍走 rt_thread 路径
  （**回到 v0.2 task 29 状态：feetech 标 HardRT 但实际 SerialRT 违约**；
  pre-1 重分类的 RtClass 字段保留无害，仅 dispatch 路径回退）
- M3 / pre-3a/b revert → trait 默认从 Err(Unsupported) 回到 v1 / v2 状态
- pre-1 / pre-2 revert → 设计修订回退（dimos 同步要相应回滚）

---

## §11 不做的事（v3 保留）

1. `robot-rpc` / `robot-viz` / `robot-policy-host` 同类改造 → v0.4
2. `ManipulatorAdapter` trait 大重构 → v0.4 真加 SoftRT adapter 时
3. B'c supervisor 联动 → 依赖 task-30 完成后的 `Event::ServiceUnhealthy`
4. 硬件级急停（继电器 / GPIO）→ §6.7 / §10.1 / task-30-post
5. servo 端 `Goal_Position` timeout fail-safe 标定 → task-30-post

---

## §12 v4 决议请求

请 review 并回复：

1. v4 整体（保留 HardRT 路径 + 新增 SerialRT executor + dispatch by `RtClass`）
   是否 approve？
2. **`RtClass::SerialRT`** 加 enum variant 走 §4.6 设计修订评审 + dimos 同步
   回写，是否同意？
3. HardRT executor 的 OutputPolicy gate **简化版**（仅"是否调 write_command"
   分支，不做 hold fallback / mode change 等复杂语义）是否接受？真 HardRT
   adapter（v0.4 EtherCAT 等）需要更多语义时再升级 rt_thread。
4. `s0_daemon` dispatch by `RtClass` 编译期 / 启动期实现：当前提议**启动期
   match**（`adapter.info().rt_class`）；要不要走编译期 trait method（`fn
   spawn_executor` 作为 trait method）？
5. v0.2 范围内 `RtClass::SoftRT` 在 `s0_daemon` 走 `Err(Unsupported)`（不实施
   spawn_io_thread）是否接受？
6. M5 行为切换点是否拆两 commit（"flag on" + "删 legacy"）？
7. 真机最低门槛 M5 smoke：
   - mock_hard_rt 走 rt_thread 路径无回归（task 29 ±17° 双夹爪重跑通过）
   - feetech 走 serial_io_thread 路径：`feetech_poke read Torque_Enable=0`
     （`mock_s1 --duration 0s` 后断扭）+ `robotctl status` 显示
     `safety_state=TorqueOffConfirmed` + 物理推夹爪
   - 同意 / 加项？

review 通过我按 pre-1 → pre-2 → pre-3 → M1-M5 顺序动手 task-30 实施。
