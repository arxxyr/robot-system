# Task-30 设计方案：state_machine ↔ adapter 执行链路

> **状态**：**草案，等 review**
> **作者**：Claude Code（2026-04-25 session）
> **背景**：`docs/handoffs/2026-04-25_1.md` §7.5.2 + §8；task 29 session 发现
> `robotctl watch` SIGTERM segfault 根因实际暴露出更深的**架构缺口**。
> **不改设计**：本文档仅**补充实现细节**，`docs/00_design.md` §0 / §1.4 / §4.9
> / §6.7 等冻结条款**一字不改**。

---

## 1. 问题陈述

### 1.1 现状

`crates/robot-s0/src/state_machine.rs` 定义了：
- **5 主态** `Safe / Ready / Enabled{sub} / Fault / Stopping`
- **4 子态** `Active / CtrlLostHold / CtrlLostZero / CtrlLostStop`
- **12 事件** `Enable / Heartbeat / HeartbeatTimeout / AdapterErr / ...`
- 完整 `try_transition` 规则表 + Guards 校验 + 单元测试全绿

运行期 state machine 跑在 `ipc_thread`：
- 收到 heartbeat → `Event::Heartbeat` → state 升级
- 心跳超时 → `Event::HeartbeatTimeout` → 进 `CtrlLostHold`
- EStop 事件 → `Event::EStop` → 进 `Safe`
- 写 iceoryx2 state topic 对外汇报当前态

### 1.2 缺口

**state machine 的 transition 不驱动 adapter 实际硬件动作**：

| 状态变化 | 期望 adapter 动作 | 当前实际 |
|---|---|---|
| `Safe → Ready`（启动 / 清 fault） | `connect()` + `set_mode(Position)` | ✅ 但是 `spawn_rt_thread` 里**一次性**做，state machine 不驱动 |
| `Ready → Enabled.Active` | `enable(true)` 防飞序列 | ✅ 但也是**一次性**（同上），state machine 不驱动 |
| `Enabled → Safe`（手动 /  stop） | `enable(false)` 断扭 | ❌ **不执行**，torque 继续 on |
| `Enabled.Active → Fault(...)` | `safety_stop()` 或 `enable(false)` | ❌ **不执行** |
| `EStop` 事件 | `safety_stop()` 立刻断扭 | ❌ **不执行** |
| `CtrlLostHold` 子态进入 | `hold_current_position()` 冻结 Goal | ⚠️ `feetech` adapter **有** helper，但没人调 |
| `cmd.mode = ControlMode::Stop` | `set_mode(Stop) → enable(false)` | ❌ **不执行**，adapter `write_command` 看到 Stop 直接 `return Ok(())` |
| `cmd.mode = ControlMode::Velocity` | `set_mode(Velocity)` 切模式 | ❌ **不执行**，adapter 只做 Position |

### 1.3 原因：adapter 所有权被 RT 闭包独占

`crates/robot-s0/src/rt_thread.rs:147-154` 注释原文：

```
// Part 1：在 spawn 之前 connect + enable，避免 RT 线程启动期做这些。
// Part 2 由 state machine 控制 enable/disable。
adapter.connect()?;
adapter.enable(true)?;
```

"Part 2 由 state machine 控制" = **已欠债**。

`adapter` 随后 `move` 进 `spawn` 闭包，rt_thread 独占所有权。ipc_thread（state machine 所在）拿不到可调的 adapter 方法。

### 1.4 影响面

- **S1 无法远程 disable torque**（robotctl / mock_s1 发 ControlMode::Stop 无效）
- **EStop 事件不断扭**（只改 state 字段，硬件状态不变）—— **违反安全**
- **`Fault` 恢复时 torque 仍然 on**（设计 §1.4 期望 Fault 进入时应进入"安全输出"）
- **bring-up 无"软件下使能"入口**（task 29 session 实测：推夹爪到闭合位用了 `feetech_poke` 裸 wire 绕过）
- **多模态控制（未来）**：v0.3 加速度 / 力控 / 动态切 Position↔Velocity 全做不了

### 1.5 优先级

- **EStop 不断扭** = **安全级 bug**，理论上 `docs/00_design.md` §6.7 "EStop 不可
  被软件清除为失忆态" 虽然指的是 *latch 持久化*，但配合"`adapter.safety_stop` 被
  调用"才构成完整闭环。当前半条腿。
- 其他（Stop mode / Velocity 模式 / Fault 断扭）属于**功能缺失**，阻塞 v0.3 功能。

---

## 2. 设计原则（硬约束）

**本设计必须同时满足**：

1. **§4.9 RT 铁律不变**：RT thread 内禁 heap alloc / 禁 `Arc::clone`/`drop` /
   禁 blocking IO / 禁 `format!`。adapter 新增路径不得让 RT 线程做串口 RTT。
2. **§6.7 systemd 为最外层 supervisor 不变**：本 task 不做进程 / supervisor 层
   改动；任何 "robot-health 启动服务" 方向走 B'c 方案 C（监控 + 决策上报）。
3. **`ManipulatorAdapter` trait 外部签名尽量不改**：保持
   `connect/disconnect/read_state/write_command/set_mode/enable/safety_stop/limits`
   8 个方法。内部实现可以换，调用方（`rt_thread`）代码零改动 / 最小改动。
4. **`panic = "abort"` 范围不变**：`robot-s0` / `robot-s1` 仍 abort；新增的 io
   helper thread 在 robot-s0 里 → 仍 abort 语义（任何 panic → 进程挂 → systemd
   重启）。
5. **现有单测 / 真机闭环 100% 向后兼容**：task 29 的 ±17° 正弦演示 + mock 单测
   必须在切换后 0 回归。
6. **允许分步合并**：设计必须支持**增量落地**（N 个小 commit），每个 commit
   都能独立通过 CI + 真机 smoke，避免大 PR 炸掉 master。

---

## 3. 架构图

### 3.1 当前（task-30 之前）

```
┌─────────────────── robot-s0 process ──────────────────┐
│                                                       │
│  ipc_thread                rt_thread (RT/SCHED_FIFO)  │
│  ┌─────────────┐           ┌─────────────────────┐    │
│  │ state_machine│          │  adapter (move 进)   │    │
│  │ (12 events) │           │  ┌───────────────┐  │    │
│  └──────┬──────┘           │  │ FeetechAdapter│  │    │
│         │ writes           │  │ + FeetechBus  │  │    │
│         ▼                  │  │ + SerialPort  │  │    │
│  iceoryx2 state topic ────>│  └──┬────────────┘  │    │
│         (only reports)     │     │               │    │
│                            │     ▼ every tick:   │    │
│  adapter ←─── NOT HERE ────┤   write_command     │    │
│  (can't reach)             │   read_state        │    │
│                            └─────────────────────┘    │
│                                                       │
│  watchdog thread (crossbeam → ipc)                    │
│  pub_thread (reads rt_snapshot → iceoryx2)            │
└───────────────────────────────────────────────────────┘
```

### 3.2 Task-30 后（推荐方案 F）

```
┌─────────────────── robot-s0 process ────────────────────────┐
│                                                             │
│  ipc_thread                rt_thread (RT/SCHED_FIFO)        │
│  ┌─────────────┐           ┌─────────────────────────────┐  │
│  │state_machine│           │ rt_hot_path (zero alloc)    │  │
│  └──────┬──────┘           │ ┌─────────────────────────┐ │  │
│         │                  │ │ read LatestCommandSlot  │ │  │
│         │ on transition:   │ │ write StateSlot         │ │  │
│         │ send AdapterReq  │ │ every 20ms tick         │ │  │
│         ▼                  │ └──────┬──────────────────┘ │  │
│  AdapterCtrlClient         │        │ reads/writes       │  │
│  (state-free facade) ─────┐│        ▼                    │  │
│  .enable(true)            ││   ┌──────────────────┐      │  │
│  .set_mode(Velocity)      ││   │ HotShared (POD)  │      │  │
│  .safety_stop()           ││   │  latest_cmd      │      │  │
│  .hold_current_position() ││   │  latest_state    │      │  │
│         │                 ││   └────┬─────────────┘      │  │
│         │ channel req     ││        │ drained by io      │  │
│         ▼                 │└────────┼────────────────────┘  │
│  ┌──────────────┐ <───────┘         │                       │
│  │ io_helper    │                   │                       │
│  │ (普通优先级) │                   │                       │
│  │ ┌──────────┐ │                   │                       │
│  │ │ Adapter  │<────────────────────┘                       │
│  │ │ (真持)   │   // io 线程独占串口                        │
│  │ │ FeetechBus│  // RTT ms 级阻塞 IO OK                    │
│  │ └──────────┘ │                                            │
│  └──────────────┘                                            │
└──────────────────────────────────────────────────────────────┘
```

**关键变化**：
- `adapter` **所有权移到 `io_helper` thread**（普通优先级，允许 blocking IO）
- RT thread 不持 `&mut adapter`；只读写 `HotShared` POD 结构（atomic / seqlock
  已建好的 `LatestCommandSlot` / seqlock snapshot，**复用当前 Part 1 类型**）
- io_helper 在后台：
  - 定期 drain HotShared.latest_cmd → 发串口
  - 定期 sync_read 硬件 → 写 HotShared.latest_state
  - 接收 state_machine 发来的 `AdapterReq`（`Enable(bool)` / `SetMode` /
    `SafetyStop` / `HoldCurrentPosition` / `Connect`）→ 阻塞执行 → 回 response
- state_machine 在 ipc_thread 里**通过 facade** 发 `AdapterReq`：看上去就是调
  `adapter.enable(false)`，实际走 channel

---

## 4. 候选方案对比

| 方案 | 说明 | 优 | 缺 | 决议 |
|---|---|---|---|---|
| **A. 拆 trait** `ManipulatorRtCtrl`（RT） + `ManipulatorCtrl`（冷） | adapter 实现两个 trait，RT 线程持前者、io 线程持后者 | 类型清晰 | **两个 impl 如何共享串口** 无干净解法（`Arc<Mutex<Bus>>` + RT lock 违反铁律） | ❌ |
| **B. 合并 rt_thread + io_thread** | 去掉 SCHED_FIFO 的 rt_thread，io 线程跑 tick loop | 单所有权 | 破坏 §4.9 RT 铁律（io 线程做串口 RTT，50 Hz deadline 必超） | ❌ |
| **C. `Arc<Mutex<Inner>>` 共享** | adapter 内部 Arc，RT 和 io 两边 lock | 代码量小 | RT 线程 `mutex.lock()` 违反 §4.9（可能阻塞、可能 alloc futex 状态）| ❌ |
| **D. rt_thread 内 drain request queue** | cold request 发到 rt_thread，在 tick 间隙执行 | adapter 独占不变 | `enable` 走串口 RTT ms 级，占 tick deadline → 50 Hz tick 超时 | ❌ |
| **E. rt / io 间 move adapter 所有权** | 冷窗口内 rt_thread pause，把 adapter move 给 io，再 move 回 | 理论可行 | pause/resume 复杂，state 不及时（pause 期间 motor 无输出） | ❌ |
| **F. adapter 在 io_helper 独占 + RT 读写共享 POD**（推荐） | RT 只读写 lock-free POD，io_helper 独占 adapter + channel 接受控制命令 | 分层清晰 / 复用现有 LatestCommandSlot / 无 RT lock | io_helper ↔ adapter 需要定义 `AdapterReq`/`AdapterResp` 枚举 + channel；对 `ManipulatorAdapter` trait 内部实现重构 | ✅ |

### 为什么选 F

1. **RT 铁律保留**：RT 侧只碰 seqlock / arc-swap（已有基础设施），**零改 RT 铁律**
2. **`ManipulatorAdapter` trait 对外**依旧有完整 8 个方法 —— 调用方（ipc_thread
   的 state_machine hook 代码）看到"正常"trait；实现层拆成 client / server 两半
3. **现有 RT hot path 代码 0 修改**（`rt_thread.rs` 的 `write_command` + `read_state`
   调用可以保持不变，只是 adapter 实例换了；也可以直接改成读写 HotShared POD）
4. **增量落地**：先只把 `enable` / `set_mode` / `safety_stop` 走新路径，
   `read_state` / `write_command` 仍走原路径（RT 独占 adapter），**分两步完成切换**

---

## 5. 详细设计（方案 F）

### 5.1 新增模块

```
crates/robot-core/src/adapter.rs         （不改，trait 定义）
crates/robot-s0/src/adapter_link/
├── mod.rs                               （pub use）
├── client.rs                            （ipc_thread / state_machine 侧 AdapterCtrlClient）
├── server.rs                            （io_helper 线程内 AdapterCtrlServer loop）
├── req.rs                               （AdapterReq / AdapterResp enum）
└── hot_shared.rs                        （RT ↔ io 的 lock-free POD 共享，复用 LatestCommandSlot / seqlock）
```

### 5.2 `AdapterReq` / `AdapterResp` 枚举

```rust
/// state_machine 发给 io_helper 的控制请求。
///
/// 非 RT 路径。channel 走 crossbeam `bounded(1)`（深度 1，避免堆积；最新命令
/// 覆盖老命令或等待上一条完成）。每请求带一个 `oneshot::Sender<AdapterResp>`
/// 让调用方同步等结果。
pub enum AdapterReq {
    Connect,
    Disconnect,
    Enable(bool),                  // 含防飞序列
    SetMode(ControlMode),
    SafetyStop,                    // disable torque
    HoldCurrentPosition,           // sync Goal=Present，不动 torque
}

pub enum AdapterResp {
    Ok,
    Err(RobotError),
}
```

### 5.3 `AdapterCtrlClient`（state_machine facade）

state_machine transition 时调用的入口：

```rust
pub struct AdapterCtrlClient {
    tx: crossbeam_channel::Sender<(AdapterReq, oneshot::Sender<AdapterResp>)>,
}

impl AdapterCtrlClient {
    /// 阻塞调用 —— 等 io_helper 执行完 adapter.enable(...) 再返回。ipc_thread
    /// 是非 RT，阻塞 500 ms 可接受（enable 真机 RTT 约 50 ms + settle 50 ms
    /// + torque_enable ~10 ms × N joint）。
    pub fn enable(&self, on: bool) -> Result<(), RobotError> {
        let (resp_tx, resp_rx) = oneshot::channel();
        self.tx.send((AdapterReq::Enable(on), resp_tx))?;
        match resp_rx.recv_timeout(Duration::from_secs(2))? {
            AdapterResp::Ok => Ok(()),
            AdapterResp::Err(e) => Err(e),
        }
    }
    // 同样的 pattern：set_mode / safety_stop / hold_current_position / connect / disconnect
}
```

**关键选择**：
- `bounded(1)` channel + `oneshot` —— 同步语义；state_machine 一条 transition
  要么等 adapter 执行完才继续，要么超时报错升级成 Fault。
- 2 秒总超时 —— 真机 handshake 最长 1s（task 28 实测）+ 防飞序列 ~100 ms +
  retry 3x × 150 ms = 最坏 ~1.5 s；给 500 ms buffer。
- 超时 → `RobotError::DeadlineExceeded` 返回 state_machine → 触发 `Event::
  AdapterErr(FaultCode::AdapterErr)` 进 Fault。

### 5.4 `AdapterCtrlServer`（io_helper loop）

```rust
pub fn run_server(
    mut adapter: Box<dyn ManipulatorAdapter + Send>,
    rx: crossbeam_channel::Receiver<(AdapterReq, oneshot::Sender<AdapterResp>)>,
    hot: Arc<HotShared>,
    stop: Arc<AtomicBool>,
) {
    while !stop.load(Ordering::Acquire) {
        // 优先处理控制命令（state_machine 最高优先级）
        if let Ok((req, resp)) = rx.try_recv() {
            let result = dispatch(&mut *adapter, req);
            let _ = resp.send(match result {
                Ok(()) => AdapterResp::Ok,
                Err(e) => AdapterResp::Err(e),
            });
            continue;
        }

        // hot path：drain HotShared.latest_cmd → adapter.write_command
        //           然后 adapter.read_state → HotShared.latest_state
        if let Some(cmd) = hot.latest_cmd.load() {
            let _ = adapter.write_command(&cmd.as_view());  // silent clamp 等 adapter 内处理
        }
        let mut state = JointStateBuf::default();
        if adapter.read_state(&mut state).is_ok() {
            hot.latest_state.publish(&state);
        }
        // 不 sleep —— read_state 本身走 sync_read，串口 RTT 即周期
        // 目标 50 Hz → read_state 20ms round-trip。实测 feetech 13 motor ≈ 5 ms
    }
}

fn dispatch(adapter: &mut dyn ManipulatorAdapter, req: AdapterReq) -> Result<(), RobotError> {
    match req {
        AdapterReq::Connect => adapter.connect(),
        AdapterReq::Disconnect => adapter.disconnect(),
        AdapterReq::Enable(on) => adapter.enable(on),
        AdapterReq::SetMode(m) => adapter.set_mode(m),
        AdapterReq::SafetyStop => adapter.safety_stop(),
        AdapterReq::HoldCurrentPosition => {
            // 仅 feetech 有这个 method，trait 没有 —— 需要向上 cast；或者给 trait
            // 加一个默认 no-op：
            //    fn hold_current_position(&mut self) -> Result<(), RobotError> { Ok(()) }
            // 见 §5.7 trait 改动讨论
            adapter.hold_current_position()
        }
    }
}
```

### 5.5 `HotShared` —— RT ↔ io 共享 POD

**复用 Part 1 已有基础**：
- `LatestCommandSlot<JointCommandBuf>`（arc-swap，RT 读 / ipc 写）
- `RtSnapshotPublisher<RtSnapshot>`（seqlock，RT 写 / pub_thread 读）

Task-30 新增：**io_helper 接管 adapter 后**，现有 rt_thread 的 `write_command`/
`read_state` 调用要转成"读 HotShared.latest_cmd / 写 HotShared.latest_state"。
实际实现有两个选择：

- **5.5.a**：**保留 rt_thread 调 adapter.write_command/read_state** —— 矛盾：
  adapter 已被 io_helper 持有，RT 线程无 adapter ref 可调
- **5.5.b（推荐）**：**rt_thread 去除 adapter 调用，改读 HotShared**。io_helper
  持续同步硬件 ↔ HotShared，rt_thread 的 tick loop 只做 cmd_reader → HotShared
  / read HotShared → state_pub 的转发。**rt_thread 实际上降级为数据通路，不再
  做 adapter RTT**

**5.5.b 的影响**：
- rt_thread 真实做的事缩水（从 "sync_read + sync_write" → "读 slot + 写 slot"），
  deadline 从 5-10 ms 骤降到 < 10 µs —— **RT 余量大增**
- io_helper 跑到实际硬件带宽的极限（feetech 50-100 Hz），与 rt_thread 解耦：
  RT tick 仍 50 Hz 但只做 snapshot 转发
- **缺点**：RT 线程和硬件实际读写有一帧延迟（io_helper 慢半拍）；50 Hz 下延迟
  ≤ 20 ms，机器人控制可接受
- **优点**：硬件异常（sync_read timeout）不阻塞 rt_thread，故障隔离更好

### 5.6 线程拓扑

| 线程 | 优先级 | 职责 | 新增？ |
|---|---|---|---|
| `ipc_thread` | 普通 | state_machine + iceoryx2 sub/pub 元数据 | 改：加 AdapterCtrlClient 并在 transition hook 里调 |
| `rt_thread` | SCHED_FIFO | tick loop，读 cmd slot / 写 state slot | 改：不再直接调 adapter，改读写 HotShared |
| `pub_thread` | 普通 | seqlock read → iceoryx2 publish | 不改 |
| `watchdog` | 普通 | 心跳超时检测 | 不改 |
| `io_helper` | 普通 | **adapter 独占，执行 AdapterReq + 硬件 RTT** | **新增** |
| `s0_io_thread`（SoftRT adapter，v0.4 加） | 普通 | 相机 / 外部传感器 | 无关 |

### 5.7 `ManipulatorAdapter` trait 改动

**最小化改动**，建议：

| 方法 | 改动 |
|---|---|
| `connect` | 不改 |
| `disconnect` | 不改 |
| `read_state` | 不改（io_helper 里调） |
| `write_command` | 不改（io_helper 里调） |
| `set_mode` | 不改 |
| `enable` | 不改 |
| `safety_stop` | 不改 |
| `limits` | 不改 |
| **`hold_current_position`（新）** | **加** 默认 no-op，feetech override；让 `AdapterCtrlClient` 用统一接口调用 |

```rust
pub trait ManipulatorAdapter {
    // ... existing 8 methods ...

    /// 冻结当前姿态：读当前 Present 写回 Goal。**不** 动 Torque。
    ///
    /// 默认 no-op（mock adapter）；`FeetechAdapter` override 走 follow-current
    /// 序列。S0 在 `CtrlLostHold` 子态自动调用。
    fn hold_current_position(&mut self) -> Result<(), RobotError> {
        Ok(())
    }
}
```

### 5.8 state_machine transition hook

在 `state_machine.rs` 里**不直接调 adapter**（保持 state_machine 纯函数），
而是在 `ipc_thread` 的 transition 处理循环里：

```rust
// ipc_thread.rs（简化示意）
fn on_transition(
    old: S0State,
    new: S0State,
    adapter_ctrl: &AdapterCtrlClient,
) -> Result<(), RobotError> {
    match (old, new) {
        (_, S0State::Safe) => adapter_ctrl.safety_stop(),
        (S0State::Safe, S0State::Ready) => {
            adapter_ctrl.connect()?;
            adapter_ctrl.set_mode(ControlMode::Position)?;
            Ok(())
        }
        (S0State::Ready, S0State::Enabled { .. }) => adapter_ctrl.enable(true),
        (S0State::Enabled { .. }, S0State::Fault(_)) => adapter_ctrl.safety_stop(),
        (
            S0State::Enabled { sub: a, .. },
            S0State::Enabled { sub: EnabledSub::CtrlLostHold, .. }
        ) if a != EnabledSub::CtrlLostHold => {
            adapter_ctrl.hold_current_position()
        }
        _ => Ok(()),
    }
}
```

**失败处理**：
- `adapter_ctrl.*()` 返回 Err → 发 `Event::AdapterErr(FaultCode::AdapterErr)` 到
  state_machine，下一 tick 进 `Fault`
- 超时（io_helper 阻塞 / 死锁）→ `RobotError::DeadlineExceeded` → 同上

---

## 6. 迁移计划（分 4 个 commit）

| # | 主题 | 工作量 | 风险 |
|---|---|---|---|
| **M1** | 新增 `adapter_link` module + `AdapterReq/Resp` / client / server skeleton + 单测 mock | 1 h | 低（纯新增代码，不动现有） |
| **M2** | `ManipulatorAdapter::hold_current_position` default no-op；`FeetechAdapter` override；单测 | 30 min | 低 |
| **M3** | `spawn_io_helper` 接管 adapter 所有权；`rt_thread` 改读 `HotShared`（原 `LatestCommandSlot` + seqlock 复用）；`spawn_rt_thread` 签名改成拿 `Arc<HotShared>` 而不是 adapter | 1.5-2 h | 中（rt_thread 行为变化，要 soak 验证） |
| **M4** | `ipc_thread` 加 transition hook 调 `AdapterCtrlClient`；`s0_daemon` wire io_helper 启停；mock_s1 发 `ControlMode::Stop` 单测 adapter 真 disable torque | 1 h | 低 |

**合计 ~3-4.5 h**（原估 2-3 h 偏乐观，实际拆清后更真实）。

每个 commit 都能独立通过 CI + smoke，**不一次性换栈**。

---

## 7. 测试覆盖

| 层次 | 用例 | 工具 |
|---|---|---|
| Unit | `AdapterCtrlClient::enable` → channel send + oneshot recv 语义 | 自编 mock server |
| Unit | `AdapterCtrlServer::dispatch` 所有 `AdapterReq` variant | `mock_hard_rt` adapter |
| Unit | `HotShared` drain 语义 | seqlock 现有测试扩展 |
| Unit | state_machine transition → 对应 `AdapterReq` 被发（用 fake client）| 新 fixture |
| Integration | mock adapter 场景：mock_s1 发 `Stop` → `robotctl status` 看到 `enabled=false` | tests/ |
| Soak | rt_guard_soak（task 26）扩：io_helper 不 leak / 不 alloc on hot path | 现有 harness |
| **Orin 真机** | `feetech_poke read Torque_Enable` 在 S1 发 `Stop` 后应返 `0` | 手动 |
| **Orin 真机** | `kill -SIGTERM s0_daemon` → adapter.safety_stop 调用 → motor torque off（示波器 / 夹爪手推） | 手动 |

---

## 8. 风险 + 回滚

### 风险 R1：io_helper 死锁 / 阻塞

- **情景**：adapter.enable 内部死循环、串口 hang。
- **检测**：`AdapterCtrlClient::*` 的 `recv_timeout(2s)` 超时。
- **反应**：上报 `Event::AdapterErr` 进 Fault；io_helper **不重启**（systemd 整
  进程重启策略统一走）。
- **回滚路径**：加 feature flag `state_machine_adapter_link`，默认 off，失败时
  可以一键回退到 Part 1 行为（spawn_rt_thread 直接持 adapter）。

### 风险 R2：RT 一帧延迟

- **情景**：5.5.b 下 rt_thread 只读 HotShared，io_helper 刷 HotShared 慢一拍。
- **量化**：50 Hz 下最大 20 ms 延迟。机器人位置控制本就有 10-30 ms 控制延时
  余量，可接受。
- **缓解**：`pub_thread` 读 state_slot 时也经同样延迟，**对外对齐**，不引入
  观测不一致。

### 风险 R3：`ControlMode::Stop` 语义变化

- **情景**：当前 `Stop` = adapter 忽略；task-30 后 `Stop` = adapter 真 disable
  torque。S1 现有代码（若有依赖旧行为）会炸。
- **缓解**：`mock_s1` / `robotctl` 都没真正用 `Stop` 走任何业务逻辑；`feetech`
  adapter 的 `set_mode(Stop)` 之前实际也走 `enable(false)`（`adapter.rs:249-250`），
  **行为一致化**而已。

### 风险 R4：`hold_current_position` 默认 no-op 的 mock

- **情景**：state_machine 进 `CtrlLostHold` 调 `adapter.hold_current_position`；
  mock adapter no-op → 内部 cmd 不变 → 还是发 "跟 cmd_reader 里的 Goal" → RT
  tick 吃到旧 Goal 重复回写 → 行为等价。
- **结论**：**对 mock 无害**，只对真硬件有 follow-current 效果。

### 回滚策略

每个 commit 都能独立 revert：
- M4 revert → ipc_thread 不调 AdapterCtrlClient，io_helper 空转，行为回退到
  Part 1
- M3 revert → rt_thread 重新持 adapter，io_helper crate 保留但无调用者
- M2 revert → `hold_current_position` 从 trait 移除
- M1 revert → `adapter_link` module 整体删除

任何一步真机回归 → 上一个 commit 是 rollback 点。

---

## 9. 不做的事（留后续 task）

1. **`robot-rpc` / `robot-viz` / `robot-policy-host` 的同类改造**：v0.4 真
   多 bus 后重新评估，可能需要各自的 io_helper。
2. **rt_thread 降级为纯数据通路后的 RT 必要性评估**：如果 rt_thread 不再做硬
   件 RTT，是否还需 `SCHED_FIFO`？留 task-30 实施后 flamegraph baseline 再回
   答。
3. **`ManipulatorAdapter` trait 重命名 / 重新分组**：现在 8 + 1 方法勉强塞
   在一个 trait，v0.4 真加 SoftRT adapter 时再拆。
4. **B'c supervisor / 重启策略联动**：依赖本 task 完成后的 `Event::Service
   Unhealthy` 真能驱动 adapter —— 见 task-31 B'c。

---

## 10. 决议请求

**请 review 并回复**：

1. 整体方向 F 是否 approve？
2. 迁移计划 M1-M4 分 commit 是否合理？
3. `hold_current_position` trait 默认 no-op 是否接受？
4. 5.5.b "rt_thread 降级为数据通路" 是否接受（一帧延迟）？还是倾向 5.5.a（强
   制 RT 直调 adapter，方案 F 退化失败）？
5. feature flag `state_machine_adapter_link` 是否需要？如要，默认 on 还是 off？

Review 通过后我按 M1-M4 顺序动手 task-30 实施。
