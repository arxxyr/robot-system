# Task-30 设计方案：state_machine ↔ adapter 执行链路

> **状态**：**v9 + errata（已批准为实施基线）**
> **历史**：v1 (`4cca734`) → v2 (`130b44b`) → v3 (`f7bdc00`) → v4 (`881e927`)
> → v5 (`46dd3c1`) → v6 (`4d016d4`) → v7 (`7334817`) → v8 (`3a89b56`) → v9
> → **v9 errata**（本文件 §0.5；不再出 v10，errata 直接写进 §0.5 + §4.4.2 + §11.2）
> **作者**：Claude Code
>
> **v9 关键修订**：v8 5 项 finding 全收 + 收敛。最关键 #1：state_machine 改
> **两阶段 transition**（plan + commit），副作用 transition 等硬件 ack 才
> commit state，消除"对外汇报 Enabled 但硬件未 enable"窗口；#2：SerialRT
> `safety_latched` 与 HardRT 对齐用 `Arc<AtomicBool>`，`clear_safety_latch`
> 同步 atomic store；#3 测试期望对齐设计（seq 空洞允许）；#4 sync_write
> 借用检查两阶段拆；#5 frame size 注释 fix。
>
> **v9 errata（实施约束，不再做大版设计）**：
> - **A1**：EStop / 任何 Emergency 事件**必须**在每个 `ipc_tick` 头部**先于**
>   pending 推进处理（preempt in-flight effectful pending），不允许"等 ack
>   完了再消化 EStop"；
> - **A2**：`pending_started_at` 在 **plan 入 pending 那一刻**立即置位（不是
>   `Submitted` 后才置），让 Busy 不断重试也计入 1s 总超时；
> - **A3**：副作用 transition 的 ack 失败（包括 `Err` 与 1s 超时）**直接**
>   `commit_fault(SafetyStopFailed)` + `set_policy(Disabled)` + `safety_stop()`
>   兜底，**不**通过 `Event::AdapterErr` 走旧 transition 表（避免再被 guard
>   挡掉造成 silent stall）。

---

## §0 v8 → v9 修订记录

User v8 review 5 项 finding，全部接受：

| # | v8 缺陷 | v9 修订 |
|---|---|---|
| **1** | 副作用 transition 先 commit state + set_policy，再等硬件 ack；窗口内 state topic = "Enabled" 但硬件未 enable | **§4.4 + §state_machine API 重写为两阶段**：`plan_transition(old, event, guards) → Option<PlannedTransition>` 不提交；副作用 transition `pending_action` 排队 → poll Ok → `state_machine.commit(...)` + `set_policy`；失败 → `Event::AdapterErr` → 进 Fault；emergency 仍立即 commit（state 进 Safe，硬件实际由 `OutputSafetyState` 反映） |
| **2** | SerialRT `clear_safety_latch` 走 normal channel + ArcSwap = void fire-and-forget；后续 Enable(true) 可能撞还没消费的 ClearLatch | **§4.5 SerialRT 与 HardRT 对齐**：`safety_latched: Arc<AtomicBool>`；trait `clear_safety_latch` 直接 atomic store false；serial_io_thread 读 atomic 拒 Enable(true)；移除 `AdapterNormalReq::ClearLatch` channel 路径 |
| **3** | §9 测试 "第二次 Busy 不消耗 seq counter" 与 §0 #1 修订记录"seq 先 fetch_add，CAS 失败 seq 废弃"自相矛盾 | **§9 改测试期望**："允许 seq 空洞，Busy 不覆盖 packed slot；fetch_add 永远 +1（即便 cas 失败该 seq 废弃）" |
| **4** | `entry_refs[i] = (*id, &data_bufs[i][..])` 同 for 循环 mut + shared borrow → borrow checker 拒 | **§7.2 两阶段拆**：阶段 1 mut 写 `data_bufs[..n]`；阶段 1 结束（drop mut borrow）；阶段 2 shared 构造 `entry_refs[..n]` → 借用检查通过；M4 编译 + clippy 验证 |
| **5** | frame size 注释 "258 = 4 header + 254 payload + 1 chksum" 与 `build_instruction:104` 公式 `6 + params.len()` 不符 | **§7.2 注释 fix**：max frame = `6 (FF FF ID LEN INST CHKSUM) + params_len`；MAX_JOINTS=32 × length=4 → params=162 → frame=168 byte；预分配 256 留余量 |

v1-v8 文档归档 git history。v9 是当前权威。**最终设计版本**：本轮所有 finding
都修完，**v9 之后建议直接进入实施**，不再追加 v10。

---

## §0.5 v9 errata（实施约束，**不另出 v10**）

v9 review 通过后，user 把 **3 条已知的安全语义问题**直接固化为实施约束 +
测试要求，避免"M7 写完再补丁"。本节是权威，**M7 实现必须满足这 3 条 + 加
对应测试**。任何与 §4.4.2 v9 原伪代码冲突的地方，以本节 + 下面"§4.4.2
errata 重写版"为准。

### A1：EStop / Emergency 必须 preempt in-flight pending

**问题**：v9 §4.4.2 ipc_tick 把"推进 pending"放在第 1 步、"消化 events"放在
第 2 步；如果 pending 卡在 1s 超时窗口内，期间 EStop 入 events 队列只能
等下下下个 tick 才能跑——违反"EStop 不可被任何 in-flight 操作阻塞"。

**约束**：
- ipc_tick **第一步**：`drain emergency events`（EStop / FaultLatched /
  SafetyStopFailed），命中即调用 `safety_stop()` + commit Emergency
  transition + `set_policy(StopTorque/Disabled)` + **强制清空 pending**
  （把 in-flight effectful 计为 cancelled，下 tick 不再 poll 它的 seq）；
- 第二步才是"推进剩下的 in-flight pending"；
- 第三步才是"plan 新 transition（pending 为空时）"。

**实现要点**：
- emergency drain 路径**不**走 plan_transition（不需要 Effectful 分支），
  直接走 §4.4.2 的 Emergency commit；
- pending cancel 时**不**回放它对应的 ack（即使 rt_thread 后来 ack 成功，
  ipc_thread 已经 commit Safe，丢弃 ack 不冲突——`safety_stop` 已经把
  policy 拍成 StopTorque）。

### A2：`pending_started_at` 在 plan 创建时置位（不是 Submitted 后）

**问题**：v9 §4.4.2 把 `pending_started_at = Some(clock.now())` 放在
`Submitted { seq }` 分支里。如果 rt_thread 一直 Busy（slot 占用从未空出），
ipc_tick 反复返回 `Busy` → `pending_started_at` 永远 None → 永远不会触发
1s 超时升 Fault。

**约束**：
- `pending_started_at` 在 **plan 入 pending 那一刻**立即置位（即 §4.4.2
  ipc_tick 第 3 步 Effectful 分支 `retry.pending = Some((plan, None))` 的
  同时 `retry.pending_started_at = Some(clock.now())`）；
- Busy 重试期间不重置 `pending_started_at`（保持原始 plan 入队时刻）；
- 1s 超时检查覆盖 None / Some(seq) 两个分支，**任何**未完成 pending 都计时。

### A3：Effectful ack 失败（Err / Timeout）直接 commit_fault，不走旧 transition 表

**问题**：v9 §4.4.2 ack 失败路径写的是 `events.push(Event::AdapterErr(...))`
+ 清 pending，等下 tick 才靠 `plan_transition` 把 AdapterErr 翻译成
`* → Fault` transition。但 `plan_transition` 受 guards 影响（task-30-pre-2
新加的 `* → Fault(SafetyStopFailed)` 也有 guard），如果某 corner case 把
event 挡住 → state_machine 卡在原态 + pending 已清 → silent stall。

**约束**：
- ack 失败（`AdapterResp::Err(_)` / 1s 超时 / `NormalReqStatus::Unsupported`）
  **直接调** `commit_fault(state, FaultCode::AdapterErr | SafetyStopFailed)`
  + `executor_ctrl.set_policy(OutputPolicy::Disabled)` + （兜底）
  `executor_ctrl.safety_stop()`；
- **不**走 `events.push(Event::AdapterErr(...))` + plan_transition；
- 失败原因映射：
  - `AdapterResp::Err(SafetyLatched)` → `FaultCode::SafetyStopFailed`
  - `AdapterResp::Err(其它)` / 1s 超时 / `Unsupported` → `FaultCode::AdapterErr`
- 失败后 pending 立即清空（同 v9）；
- M7 仍保留 `commit_fault` API 给其它路径（如 watchdog soft-deadline 触发）
  使用，但 ack 失败路径**不依赖** event queue。

### v9 errata 强制测试（M7）

实施 task #6（M7）必须包含以下 3 条单测，**红灯不允许 merge**：

| 测试 | 验证 | 期望 |
|---|---|---|
| `test_estop_preempts_in_flight_pending` | A1：在 effectful pending 卡在 Submitted 等 ack 时注入 EStop event | EStop 在**当前 tick** 内 commit Safe + `set_policy(StopTorque)`，pending 被清空（即使后续 rt_thread ack 也不影响 state） |
| `test_busy_total_timeout_1s` | A2：rt_thread 始终返回 Busy（不消费 slot），1s 后必须 commit Fault | 入 pending 后 ≤ 1.05s 内 `state == Fault(AdapterErr)` + `policy == Disabled`；**不**允许永久卡 Busy |
| `test_effectful_failure_direct_fault` | A3：ack 返回 `Err(SafetyLatched)` | 当前 tick 内 `state == Fault(SafetyStopFailed)` + `policy == Disabled`；**不**经过任何 `Event::AdapterErr` 入队 |

---

## §1 问题陈述（v4-v8 保留）

详 v8 §1 / v7 §1。三类 adapter / 三条 executor / 双重缺口 / Schema 已欠债 /
SIGKILL ≠ task-30 完成。

---

## §2 设计原则（v8 23 条 + v9 加 2）

1-23：v8 全保留。

**24（v9 新加）**：**副作用 transition 两阶段提交**。state_machine 区分 "纯
状态切换"（无副作用，立即 commit）和 "带 adapter 动作的副作用 transition"
（需 commit-after-ack）。后者：`plan_transition` 计算下一态 + 需要的 action；
ipc_thread 提交 action 后 poll Ok → commit；失败 → 进 Fault。**禁止** 在硬件
ack 前 commit 副作用 transition 的新 state / 新 policy 到对外。

**25（v9 新加）**：**latch 字段语义跨 executor 完全一致**。HardRT 和 SerialRT
都用 `Arc<AtomicBool>`；`clear_safety_latch` 调用语义 = atomic store false；
**禁止** 把 latch 清除做成 channel 异步动作（同步 + 立即 visible 才能保证
后续 Enable(true) 看到正确值）。

---

## §3 架构图（v8 保留 + v9 修 ipc retry 为两阶段）

```
┌─── robot-s0 ──────────────────────────────────────────────────────────┐
│                                                                       │
│  ipc_thread                                                           │
│  ┌─────────────────────────────────────────────────────────────────┐  │
│  │ each ipc tick:                                                  │  │
│  │ 1. 推进 in-flight pending（v9 §4.4 两阶段第二阶段）：             │  │
│  │    poll(seq) → Ok → state_machine.commit(plan) + set_policy      │  │
│  │              → Err → Event::AdapterErr → state_machine.commit_   │  │
│  │                       fault() + set_policy(Disabled) + safety_   │  │
│  │                       stop                                        │  │
│  │              → None → 等下 tick                                   │  │
│  │    pending 期间 **不开新 transition**                             │  │
│  │ 2. plan_transition (no-op if pending != None)：                    │  │
│  │    let plan = state_machine.plan_transition(state, event, guards) │  │
│  │    match plan.kind {                                              │  │
│  │      Pure => commit + set_policy（无副作用，立即）                 │  │
│  │      Emergency => safety_stop() (fire-and-forget) + commit        │  │
│  │                   立即（state 进 Safe，硬件由 OutputSafetyState   │  │
│  │                   反映）                                           │  │
│  │      Effectful { action } => pending = Some((plan, None))         │  │
│  │                              下 tick 提交 + poll                  │  │
│  │    }                                                              │  │
│  └─────────────────────────────────────────────────────────────────┘  │
│                                                                       │
│  ╔═══════════════════════════════════════════════════════════════════╗│
│  ║  HardRtCtrlClient (单 packed AtomicU64) │ SerialIoCtrlClient       ║│
│  ║  ┌─────────────────────────────────────┐│┌─────────────────────┐  ║│
│  ║  │ policy: AtomicU8                    │││ policy: AtomicU8    │  ║│
│  ║  │ emergency_action: AtomicU8          │││ emergency_tx: ch    │  ║│
│  ║  │ pending_normal: AtomicU64 (a/r/s)   │││ estop_atomic: AB    │  ║│
│  ║  │ seq_counter: AtomicU64              │││ pending_normal:     │  ║│
│  ║  │ last_completed: AtomicU64 (s/code)  │││   ArcSwap (普通)    │  ║│
│  ║  │ safety_latched: AtomicBool          │││ last_resp: ArcSwap  │  ║│
│  ║  │                                     │││ safety_latched:     │  ║│
│  ║  │                                     │││   Arc<AtomicBool>   │  ║│
│  ║  │                                     │││   ← v9 与 HardRT 对齐│  ║│
│  ║  └────────────┬────────────────────────┘│└──────────┬──────────┘  ║│
│  ║               ▼                          │           ▼              ║│
│  ║  rt_thread (SCHED_FIFO)                  │  serial_io_thread       ║│
│  ║   1. emergency.swap → safety_stop_rt     │   select! ...            ║│
│  ║      → drain pending → write Preempted   │   safety_latched 同读    ║│
│  ║   2. pending.swap → 二次 latch 检查      │     atomic（不走 channel）║│
│  ║   3. policy gate                         │                          ║│
│  ║   4. read_state + publish                │                          ║│
│  ╚══════════════════════════════════════════════════════════════════╝│
└───────────────────────────────────────────────────────────────────────┘
```

---

## §4 ExecutorCtrlClient trait（v8 5 method 保留 + v9 SerialRT latch 对齐）

### 4.1 trait 定义（v8 不变）

5 method：`set_policy / safety_stop / request_normal / poll_last_normal_resp /
clear_safety_latch`。详 v8 §4.1。

### 4.2 HardRT 实现（v8 单 packed AtomicU64 保留）

详 v8 §4.2 全保留。`pending_normal: AtomicU64` packed (8/8/48) + `seq_counter`
+ `last_completed` packed + `safety_latched: AtomicBool`。`request_normal` 用
fetch_add 先分配 seq → CAS 占用 → 失败 seq 废弃。

### 4.3 rt_thread 主循环（v8 emergency drain pending 保留）

详 v8 §4.3 全保留。

### 4.4 ipc_thread 两阶段 transition 协议（**v9 重写**）

#### 4.4.1 state_machine 新 API

```rust
/// 计算 transition，**不**提交。返回**计划**，由调用方决定何时 commit。
pub fn plan_transition(
    state: S0State,
    event: Event,
    guards: &Guards,
) -> Option<PlannedTransition>;

#[derive(Clone, Debug)]
pub struct PlannedTransition {
    pub new_state: S0State,
    pub new_policy: OutputPolicy,
    pub kind: TransitionKind,
}

#[derive(Clone, Debug)]
pub enum TransitionKind {
    /// 无副作用 transition（如 Stopping / Fault clear / 子态切换）。
    /// 立即 commit + set_policy。
    Pure,
    /// 紧急 transition：必须立即 commit + safety_stop（fire-and-forget）。
    /// 例：任意态 + Event::EStop → Safe。
    /// state 立刻反映意图（Safe）；硬件实际由 OutputSafetyState 反映
    /// （StopCommanded → TorqueOffConfirmed / StopFailed）。
    Emergency,
    /// 副作用 transition：需要先调 adapter 动作，等硬件 ack 才 commit。
    /// 例：Ready → Enabled.Active 需 enable_rt(true)；Enabled → Safe 需
    /// enable_rt(false)（非 emergency 路径，正常 Stop 走这里）。
    Effectful { action: AdapterNormalReq },
}

/// commit 计划好的 transition。**仅在** ack 成功后调（ipc_thread 责任）。
pub fn commit_transition(state: &mut S0State, plan: &PlannedTransition);

/// 副作用 transition adapter 失败 → fire AdapterErr 升 Fault。
pub fn commit_fault(state: &mut S0State, fault: FaultCode);
```

#### 4.4.2 ipc_thread retry coordinator

```rust
pub struct IpcRetryState {
    /// (plan, seq_if_submitted)：
    /// - `Some((plan, None))`：plan 已 reserved，下 tick 提交
    /// - `Some((plan, Some(seq)))`：已 Submitted，等 poll 返 result
    /// - `None`：无 in-flight，可以 plan 新 transition
    pending: Option<(PlannedTransition, Option<u64>)>,
    /// in-flight 起始时刻（超时 1s 升 Fault）
    pending_started_at: Option<MonoRawNs>,
}

fn ipc_tick(
    retry: &mut IpcRetryState,
    state_machine_state: &mut S0State,
    guards: &mut Guards,
    executor_ctrl: &dyn ExecutorCtrlClient,
    clock: &dyn RobotClock,
    events: &mut EventQueue,
) {
    // 1. 推进 in-flight（仅 Effectful 才会 reach 这里；Emergency / Pure
    //    在 commit 时直接清掉 pending）
    if let Some((plan, seq_opt)) = retry.pending.clone() {
        match seq_opt {
            None => {
                // 第一次或上次 Busy 重试
                let req = match &plan.kind {
                    TransitionKind::Effectful { action } => action.clone(),
                    _ => unreachable!("non-Effectful in pending"),
                };
                match executor_ctrl.request_normal(req) {
                    NormalReqStatus::Submitted { seq } => {
                        retry.pending = Some((plan, Some(seq)));
                        retry.pending_started_at = Some(clock.now());
                    }
                    NormalReqStatus::Busy => {
                        // 继续等下 tick；不前进
                        return;
                    }
                    NormalReqStatus::Unsupported(_msg) => {
                        events.push(Event::AdapterErr(FaultCode::AdapterErr));
                        retry.pending = None;
                        retry.pending_started_at = None;
                    }
                }
            }
            Some(seq) => {
                // 已 Submitted，等 poll
                match executor_ctrl.poll_last_normal_resp(seq) {
                    None => {
                        // 还没执行完；超时检查
                        if let Some(start) = retry.pending_started_at
                            && clock.now() - start > DurationNs::from_secs(1)
                        {
                            events.push(Event::AdapterErr(FaultCode::AdapterErr));
                            retry.pending = None;
                            retry.pending_started_at = None;
                        }
                        return;
                    }
                    Some(AdapterResp::Ok) => {
                        // **v9 关键**：硬件 ack → 现在才 commit state + set_policy
                        commit_transition(state_machine_state, &plan);
                        executor_ctrl.set_policy(plan.new_policy);
                        retry.pending = None;
                        retry.pending_started_at = None;
                    }
                    Some(AdapterResp::Err(e)) => {
                        events.push(Event::AdapterErr(map_err_to_fault(e)));
                        retry.pending = None;
                        retry.pending_started_at = None;
                    }
                }
            }
        }
    }

    // 2. plan_transition（仅在 pending 为空时）
    if retry.pending.is_some() {
        return;
    }
    while let Some(event) = events.pop() {
        let plan = match plan_transition(*state_machine_state, event, guards) {
            Some(p) => p,
            None => continue,
        };
        match plan.kind {
            TransitionKind::Pure => {
                commit_transition(state_machine_state, &plan);
                executor_ctrl.set_policy(plan.new_policy);
            }
            TransitionKind::Emergency => {
                // **emergency 立即 commit**（state 进 Safe，让外部知道意图）；
                // 硬件实际由 safety_stop fire-and-forget + OutputSafetyState 反映
                executor_ctrl.safety_stop();
                commit_transition(state_machine_state, &plan);
                executor_ctrl.set_policy(plan.new_policy); // 通常 Disabled / StopTorque
            }
            TransitionKind::Effectful { .. } => {
                // **副作用：plan 入 pending；下 tick 提交 + poll**
                retry.pending = Some((plan, None));
                break; // 一个 tick 只排一个 pending
            }
        }
    }
}
```

**核心保证**：
- 副作用 transition：state / policy / state topic 反映"已对齐硬件"的状态；
  ack 失败 → 进 Fault，**永不**虚假报告 Enabled
- emergency：state 立即进 Safe 让上游知道意图；硬件状态由独立 `safety_state`
  字段反映（意图 vs 实际正交）
- 1s 超时升 Fault 防止 rt_thread / serial_io_thread hang 永久卡

#### 4.4.3 ipc_tick errata 重写版（**§0.5 A1/A2/A3 实施版**，M7 以本版为准）

> 本节是 §4.4.2 的**最终实施版**。§4.4.2 原伪代码保留作历史对比；M7 落地
> 必须按本节实现，并通过 §0.5 强制测试。

```rust
fn ipc_tick(
    retry: &mut IpcRetryState,
    state_machine_state: &mut S0State,
    guards: &mut Guards,
    executor_ctrl: &dyn ExecutorCtrlClient,
    clock: &dyn RobotClock,
    events: &mut EventQueue,
) {
    // === Step 1: A1 — emergency / safety events 优先 preempt 任何 in-flight ===
    //
    // EStop / FaultLatched / SafetyStopFailed 必须在 tick 头部消化，不能
    // 被 effectful pending 阻塞 1s。命中即清 pending、commit Emergency、
    // 调 safety_stop()。后续 rt_thread 即使 ack 成功也不再 commit（pending
    // 已 cancelled）。
    if let Some(emergency_event) = events.drain_emergency() {
        // drain_emergency: 从 EventQueue 摘出第一个 EStop/FaultLatched/
        //                  SafetyStopFailed；其它事件保留
        if let Some(plan) =
            plan_transition(*state_machine_state, emergency_event, guards)
        {
            debug_assert!(matches!(plan.kind, TransitionKind::Emergency));
            executor_ctrl.safety_stop();                 // fire-and-forget
            commit_transition(state_machine_state, &plan);
            executor_ctrl.set_policy(plan.new_policy);   // StopTorque/Disabled
        }
        // 强制清空任何 in-flight pending：即使后来 rt_thread ack，丢弃即可
        retry.pending = None;
        retry.pending_started_at = None;
        // 本 tick 不再处理后续（emergency 已经把状态拍到 Safe；下 tick 继续）
        return;
    }

    // === Step 2: 推进 in-flight effectful pending（仅 Effectful） ===
    if let Some((plan, seq_opt)) = retry.pending.clone() {
        // A2：超时检查覆盖**所有**未完成 pending（None 与 Some(seq) 都查）
        if let Some(start) = retry.pending_started_at
            && clock.now() - start > DurationNs::from_secs(1)
        {
            // A3：直接 commit_fault，**不**走 events.push(AdapterErr)
            commit_fault(state_machine_state, FaultCode::AdapterErr);
            executor_ctrl.set_policy(OutputPolicy::Disabled);
            executor_ctrl.safety_stop(); // 兜底
            retry.pending = None;
            retry.pending_started_at = None;
            return;
        }
        match seq_opt {
            None => {
                // 第一次或 Busy 重试 —— 注意 pending_started_at 已在 Step 3
                // plan 入队时设过，Busy 重试**不**重置
                let req = match &plan.kind {
                    TransitionKind::Effectful { action } => action.clone(),
                    _ => unreachable!("non-Effectful in pending"),
                };
                match executor_ctrl.request_normal(req) {
                    NormalReqStatus::Submitted { seq } => {
                        retry.pending = Some((plan, Some(seq)));
                        // pending_started_at 不动（A2：保持 plan 入队时刻）
                    }
                    NormalReqStatus::Busy => {
                        // 等下 tick；超时由上面的 Step 2 头部检查兜底
                        return;
                    }
                    NormalReqStatus::Unsupported(_msg) => {
                        // A3：直接 commit_fault
                        commit_fault(state_machine_state, FaultCode::AdapterErr);
                        executor_ctrl.set_policy(OutputPolicy::Disabled);
                        executor_ctrl.safety_stop();
                        retry.pending = None;
                        retry.pending_started_at = None;
                        return;
                    }
                }
            }
            Some(seq) => {
                match executor_ctrl.poll_last_normal_resp(seq) {
                    None => return, // 等下 tick；超时由 Step 2 头部检查
                    Some(AdapterResp::Ok) => {
                        // ack 成功 —— 现在才 commit state + set_policy
                        commit_transition(state_machine_state, &plan);
                        executor_ctrl.set_policy(plan.new_policy);
                        retry.pending = None;
                        retry.pending_started_at = None;
                    }
                    Some(AdapterResp::Err(e)) => {
                        // A3：直接 commit_fault；不走 Event::AdapterErr
                        let fault = match e {
                            AdapterErr::SafetyLatched => FaultCode::SafetyStopFailed,
                            _ => FaultCode::AdapterErr,
                        };
                        commit_fault(state_machine_state, fault);
                        executor_ctrl.set_policy(OutputPolicy::Disabled);
                        if matches!(fault, FaultCode::SafetyStopFailed) {
                            executor_ctrl.safety_stop(); // 兜底
                        }
                        retry.pending = None;
                        retry.pending_started_at = None;
                    }
                }
            }
        }
    }

    // === Step 3: plan_transition（pending 为空时） ===
    if retry.pending.is_some() {
        return;
    }
    while let Some(event) = events.pop() {
        let plan = match plan_transition(*state_machine_state, event, guards) {
            Some(p) => p,
            None => continue,
        };
        match plan.kind {
            TransitionKind::Pure => {
                commit_transition(state_machine_state, &plan);
                executor_ctrl.set_policy(plan.new_policy);
            }
            TransitionKind::Emergency => {
                // 兜底：万一 events 里漏出 emergency（不该发生，drain_emergency
                // 应该在 Step 1 都拿走），这里也走 emergency commit
                executor_ctrl.safety_stop();
                commit_transition(state_machine_state, &plan);
                executor_ctrl.set_policy(plan.new_policy);
            }
            TransitionKind::Effectful { .. } => {
                // A2：plan 入 pending 同时立即置 pending_started_at
                retry.pending = Some((plan, None));
                retry.pending_started_at = Some(clock.now());
                break; // 一个 tick 只排一个 pending
            }
        }
    }
}
```

**与 §4.4.2 v9 原版差异（errata diff）**：
1. 新增 Step 1（A1）：`events.drain_emergency()` 头部消化 emergency，命中即
   清 pending + commit Emergency；
2. Step 2 头部加 1s 总超时检查（A2 配套）；ack 失败 / 超时 / Unsupported
   全改成**直接** `commit_fault` + `set_policy(Disabled)` + 兜底
   `safety_stop`（A3），不再 `events.push(Event::AdapterErr)`；
3. Step 3 Effectful 分支：`pending_started_at` 与 `pending` 同 tick 置位
   （A2）。

`EventQueue::drain_emergency()` 是新加 method（Step 1 用），实现：
- 内部线性扫一次队列
- 移除并返回第一个 `Event::EStop` / `Event::FaultLatched` /
  `Event::SafetyStopFailed`（如有）
- 其它事件原序保留

`commit_fault` 是 §4.4.1 新加 API，参数 `(state, FaultCode)`，直接把
state 拍到对应 `Fault(code)` 子态，不走 plan_transition。

### 4.5 SerialRT 实现（**v9 与 HardRT latch 对齐**）

```rust
pub struct SerialIoCtrlInner {
    policy: AtomicU8,
    emergency_tx: crossbeam_channel::Sender<AdapterEmergencyReq>,
    estop_atomic: AtomicBool, // emergency 兜底
    pending_normal: arc_swap::ArcSwap<Option<AdapterNormalReq>>,
    last_resp: arc_swap::ArcSwap<Option<AdapterResp>>,
    seq_counter: AtomicU64,
    last_completed_seq: AtomicU64,
    /// **v9 修订（§0 #2）**：与 HardRT 完全对齐。trait `clear_safety_latch`
    /// 直接 atomic store false（**不**走 normal channel ClearLatch）；
    /// serial_io_thread tick 头部读 atomic 决定 Enable(true) 是否拒
    safety_latched: Arc<AtomicBool>,
}

impl ExecutorCtrlClient for SerialIoCtrlClient {
    // set_policy / safety_stop / request_normal / poll_last_normal_resp 同 v7

    fn clear_safety_latch(&self) {
        // **v9**：直接 atomic store；同步 + 立即 visible
        self.inner.safety_latched.store(false, Ordering::Release);
    }
}
```

`serial_io_thread` 主循环（v6 §6 保留 + v9 加 latch 同步检查）：

```rust
fn handle_safety_stop(...) {
    let result = adapter.safety_stop(); // feetech impl 走 fast path
    state.safety_latched_local = true;
    ctx.inner.safety_latched.store(true, Ordering::Release); // **v9** 同步置位
    // ... 设 safety_state = StopCommanded ...
}

fn handle_normal(req, state, ctx) {
    let latched = ctx.inner.safety_latched.load(Ordering::Acquire);
    if matches!(req, AdapterNormalReq::Enable(true)) && latched {
        return Err(RobotError::Unsupported("Enable(true) refused: safety latched"));
    }
    // ... 正常处理 ...
}
```

**移除** v8 提及的 `AdapterNormalReq::ClearLatch` channel 路径 —— `clear_safety_
latch` 不走 channel，只是 atomic store。

---

## §5 OutputPolicy / OutputSafetyState / OutputSafetyProbe / RtSafeCtrl

详 v7-v8 §5（保留）：6 OutputPolicy + AdapterUncertain=5 + OutputSafetyProbe
trait + RtSafeCtrl 子 trait（Mock 实现 / Feetech 不实现）。

---

## §6 SerialRT confirm_torque_off 路径（v6/v7/v8 保留）

详 v8 §6。`StopCommanded → confirm → all_off / any_off / none_off` 三分支。

---

## §7 Feetech `safety_stop_fast` + 预分配 frame API（**v9 修 borrow + 注释**）

### 7.1 当前状态

`enable(false)` 13 motor × 3 retry × 150ms = ~5.85s。

### 7.2 v9 实现：两阶段 borrow + 注释 fix

**v8 §7.2 错误**：

```rust
// 错：data_bufs[i] 同 for 循环 mut + shared borrow → borrow checker 拒
for (i, (id, value)) in entries.iter().enumerate() {
    split_bytes_le(encoded, length_usize, &mut data_bufs[i][..])?;  // mut
    entry_refs[i] = (*id, &data_bufs[i][..length_usize]);            // shared
}
```

**v9 两阶段拆**（mut 阶段完成才 shared 借）：

```rust
impl<T: SerialTransport> FeetechBus<T> {
    /// **v9 修**：阶段 1 mut 写 data_bufs，阶段 2 shared 构造 entry_refs。
    /// 借用检查通过；运行期不 alloc（栈数组）。
    ///
    /// `frame_buf` 必须 `>= MAX_FRAME_LEN` byte。MAX_FRAME_LEN 公式：
    ///   = 6 (FF FF ID LEN INST CHKSUM) + max_params
    ///   max_params = 2 (addr + length) + N × (1 + length_byte)
    ///   N=MAX_JOINTS=32, length=4 → max_params = 162 → MAX_FRAME_LEN = 168
    /// 调用方预分配 `[0u8; 256]` 留余量。
    pub fn sync_write_by_id_into(
        &mut self,
        reg_name: &'static str,
        entries: &[(u8, i32)],
        frame_buf: &mut [u8],
    ) -> Result<(), FeetechError> {
        let reg = get_register(reg_name)?;
        let length_usize = usize::from(reg.length); // **v9 fix**：字段名 length
        let n = entries.len();
        if n > robot_core::adapter::MAX_JOINTS {
            return Err(FeetechError::ByteLengthMismatch {
                expected: robot_core::adapter::MAX_JOINTS,
                actual: n,
            });
        }

        // 栈数组（无 alloc）
        let mut data_bufs: [[u8; 4]; robot_core::adapter::MAX_JOINTS] =
            [[0; 4]; robot_core::adapter::MAX_JOINTS];

        // **阶段 1**：mut 写 data_bufs[..n]（无借用冲突，每个 i 独立）
        for (i, (_id, value)) in entries.iter().enumerate() {
            let encoded = encode_value(reg_name, *value)?;
            split_bytes_le(encoded, length_usize, &mut data_bufs[i][..length_usize])?;
        }
        // 阶段 1 结束 —— mut borrow drop

        // **阶段 2**：shared 构造 entry_refs[..n]；data_bufs 不再 mut，
        // 借用检查通过
        let mut entry_refs: [(u8, &[u8]); robot_core::adapter::MAX_JOINTS] =
            [(0, &[]); robot_core::adapter::MAX_JOINTS];
        for (i, (id, _)) in entries.iter().enumerate() {
            entry_refs[i] = (*id, &data_bufs[i][..length_usize]);
        }

        let n_bytes = build_sync_write(reg.addr, reg.length, &entry_refs[..n], frame_buf)?;
        self.transport.write_all(&frame_buf[..n_bytes])
    }
}
```

**`protocol::build_sync_write` 内部** v8 修订保留：`vec![0u8; total_params]`
→ 栈 `[u8; MAX_PARAMS]`（与现有 `build_write` 模式对齐）。详 v8 §7.2 (a)。

**`FeetechAdapter::safety_stop_fast`**（v8 保留 + 注释 fix）：

```rust
pub struct FeetechAdapter<T: SerialTransport> {
    // ... 现有 ...
    safety_stop_entries: Vec<(u8, i32)>,  // connect 时 with_capacity + push
    /// **v9 注释 fix**：
    ///   max frame = 6 (FF FF ID LEN INST CHKSUM) + params_len
    ///   params_len = 2 (addr+length) + N × (1+length_byte)
    ///   MAX_JOINTS=32 × length=4 → params=162 → frame=168 byte
    ///   预分配 256 留余量
    safety_stop_frame_buf: Vec<u8>,
    confirm_torque_buf: Vec<i32>,
}

impl FeetechAdapter {
    pub fn safety_stop_fast(&mut self) -> Result<(), FeetechError> {
        self.bus.sync_write_by_id_into(
            "Torque_Enable",
            &self.safety_stop_entries,    // (id, 0) 已 connect 时填好
            &mut self.safety_stop_frame_buf, // capacity 256，运行期不 alloc
        )
    }

    pub fn connect(&mut self) -> Result<(), RobotError> {
        // ... handshake + joint_id 缓存 ...
        self.safety_stop_entries.clear();
        for &id in &self.joint_ids {
            self.safety_stop_entries.push((id, 0));
        }
        self.safety_stop_frame_buf.resize(256, 0);
        self.confirm_torque_buf.resize(self.joint_ids.len(), 0);
        Ok(())
    }
}
```

### 7.3 测试

- `cargo build` + `cargo clippy` 编译通过（验证 borrow checker pass）
- `assert_no_alloc::assert_no_alloc(|| adapter.safety_stop_fast())` 不 panic
- mock transport：wire bytes 含 broadcast id `0xFE` + `Torque_Enable.addr=40` +
  全 0 data；MAX_JOINTS=32 × length=4 时 frame ≤ 168 byte
- 时延 ≤ 5ms（mock 即时）

---

## §8 SIGTERM graceful + SIGKILL blocking gap（v5 保留）

详 v5 §9。task-30 完成 ≠ v0.2 SLA 通过。

---

## §9 测试覆盖（v8 + v9 修订）

继承 v8 全部 + v9 修订：

| 用例 | 验证 |
|---|---|
| `pack_normal` / `unpack_normal` / `pack_completed` round-trip 全 valid 范围 | v8 #1 |
| `request_normal` 两次连续：第一返 `Submitted{seq=N}`；第二返 `Busy`；**packed slot 仍是第一次的 (action, arg, N)；seq_counter = N+1（允许空洞）** | **v9 #3 修测试期望** |
| rt_thread `swap` 出 packed → action+arg+seq 一致；写 `last_completed` 用同一 seq | v8 |
| emergency 后 RT 立即 drain pending → 写 `RESP_ERR_PREEMPTED_BY_EMERGENCY`；`Enable(true)` 不被执行 | v8 #2 |
| 二次 latch check：emergency latch 后**新提交**的 Enable(true)（client 端没看到 latch 已置位的瞬间）→ rt_thread 写 `RESP_ERR_REJECTED_BY_LATCH` | v8 #2 |
| **v9 两阶段 transition**：`plan_transition` 返回 `Effectful` → ipc 排 pending → submit → poll Ok → 才 commit state + set_policy；commit 前对外 state topic **仍是旧 state** | **v9 #1** |
| `plan_transition` 返回 `Effectful` → adapter Err → fire AdapterErr → 进 Fault，**不**升级到目标 state | v9 #1 |
| `plan_transition` 返回 `Pure` → 立即 commit（无 in-flight） | v9 #1 |
| `plan_transition` 返回 `Emergency` → safety_stop fire-and-forget + commit 立即 + state 进 Safe；`safety_state` 由 RT 异步更新 | v9 #1 |
| in-flight pending 期间收到新 event → 不立即排新 pending（事件入队等下次 tick） | v9 #1 |
| in-flight pending 超过 1s → 自动升 Fault | v8 / v9 |
| **v9 SerialRT `clear_safety_latch`**：trait method 立即 atomic store false；后续 Enable(true) 不被拒 | **v9 #2** |
| SerialRT emergency 后 `safety_latched=true`，立即调 Enable(true) → serial_io_thread 读 atomic 拒 | v9 #2 |
| `state_machine` `Event::ClearEStop` 处理顺序：清 EStop latch → 清 Fault → `clear_safety_latch` → 才允许 Enable(true) | v8 |
| **v9 `sync_write_by_id_into` 两阶段 borrow**：`cargo build -p robot-adapter-feetech` + `cargo clippy` 编译通过 | **v9 #4** |
| `sync_write_by_id_into` `assert_no_alloc::assert_no_alloc(...)` 不 panic | v8 |
| `sync_write_by_id_into` wire bytes 与 dimos 期望对照（13 motor + 1 byte reg + broadcast id） | v8 |
| `register.length` 字段引用（不是 `size`） | v8 |
| **v9 frame size 公式注释正确**：MAX_JOINTS=32 × length=4 = 168 byte ≤ 256 buf；MAX_JOINTS=32 × length=1 = 102 byte ≤ 256 buf；超 buf 返 BufferTooSmall | **v9 #5** |
| **【blocking gap】** SIGKILL → torque off **不通过** | 不假装 |

---

## §10 风险（v3-v8 + v9 加 R18）

### v9 R18：两阶段 transition 增加 ipc tick 延迟

**风险**：副作用 transition 必须等硬件 ack（rt_thread 至少一个 tick = 20 ms 才
能完成 Enable(true)）；ipc tick 默认 500 µs poll 一次。最坏 transition 延迟：
- HardRT：20 ms（一个 rt tick） + 500 µs（ipc poll）≈ 21 ms
- SerialRT：feetech enable(true) 50-100 ms + ipc poll ≈ 100 ms

vs v8 立即 commit：state 切换 0 延迟。

**缓解**：
- 21-100 ms 远小于 verification SLA（S1 crash → S0 safe output < 250 ms）
- 状态切换延迟换"硬件状态对齐"是合理 trade-off
- emergency 路径**不**有此延迟（仍立即 commit）—— 安全关键路径未被影响

---

## §11 迁移计划

### 11.1 前置 task-30-pre（v7 保留）

详 v7 §11.1。pre-1a/b/c/d + pre-2 + pre-3a/b。

### 11.2 主体 M1-M7（v8 保留 + v9 改 M7）

| # | 主题 | 工作量 | 依赖 |
|---|---|---|---|
| **M1** | `ExecutorCtrlClient` trait (5 method) + `OutputPolicy/SafetyState` AtomicU8 + `AdapterNormalReq` + `NormalReqStatus` + atomic 编码常量；pack/unpack helper + 单测 | 1 h | pre-3a/b |
| **M2** | `RtSafeCtrl` 子 trait + `MockAdapter` 实现 + `assert_no_alloc` 单测；`write_command(Stop)` 合约 doc | 1 h | M1 |
| **M3** | `OutputSafetyProbe` trait + `FeetechAdapter` / `MockAdapter` 实现 | 1 h | pre-1 |
| **M4** | **v8/v9 重写**：`protocol::build_sync_write` 内部 vec → 栈数组；`bus::sync_write_by_id_into` slice API + 两阶段 borrow；`safety_stop_fast` + 预分配；`assert_no_alloc` + 借用检查单测 | 1.5 h | M3 |
| **M5** | `HardRtCtrlClient` (单 packed AtomicU64 + safety_latched AtomicBool) + rt_thread 主循环 (emergency drain + 二次 latch check) + `clear_safety_latch` trait method 实现 | 2 h | M1 / M2 |
| **M6** | `SerialIoCtrlClient` (含 v9 共享 `Arc<AtomicBool> safety_latched`) + `serial_io_thread` skeleton + confirm_torque_off + clear_safety_latch atomic store；feature flag off | 2.5 h | M1 / M3 / M4 / pre-3b |
| **M7** | **v9 errata 实施版**：`s0_daemon` dispatch by RtClass；ipc_thread 按 §4.4.3 errata 三步式（Step 1 emergency drain → Step 2 推进 pending + 1s 超时 → Step 3 plan）；state_machine 加 `plan_transition` / `commit_transition` / `commit_fault` API；`EventQueue::drain_emergency` 新 method；删 legacy bootstrap；feature flag default on；**强制带 §0.5 三测**（`test_estop_preempts_in_flight_pending` / `test_busy_total_timeout_1s` / `test_effectful_failure_direct_fault`） | 3 h | M5 / M6 / pre-2 |

**总 ~12 h**（v8 估 11h；v9 加两阶段 transition +0.5h；v9 errata M7 +0.5h
 写 §0.5 三测）。

---

## §12 v9 决议请求

请 review 并回复：

1. v9 整体（两阶段 transition + SerialRT atomic latch + sync_write 两阶段
   borrow + frame 注释 fix + seq 测试期望对齐）approve？
2. **`PlannedTransition` 三分类**（Pure / Emergency / Effectful）是否覆盖完整
   state_machine 12 events？需要新加 variant 吗？
3. **emergency `Pure` 分支**（emergency 立即 commit state，不等硬件 ack）是否
   接受 R18 描述的风险？意图 (state) 与硬件实际 (safety_state) 正交呈现给
   外部，是否清晰？
4. **SerialRT safety_latched 改 `Arc<AtomicBool>` 与 HardRT 对齐** 同意？
5. **plan_transition / commit_transition API** 是 state_machine 现有 `try_
   transition` 的扩展（保留 try_transition 给单测/其他场景）还是替换？
6. M4 / M7 改动（M7 +0.5h 加两阶段 transition）总 11.5h 合理？
7. M7 拆两 commit（"flag on" + "删 legacy"）？
8. 真机最低门槛 M7 smoke：v8 全套 + v9 加：
   - 副作用 transition 期间 `robotctl status` 看到 state 仍是旧 state（直到
     ack）
   - SerialRT `clear_safety_latch` 后立即 Enable(true) 不被拒
   - `sync_write_by_id_into` 在 borrow checker 下编译通过
   - 同意 / 加项？

review 通过我按 pre-1 → pre-2 → pre-3 → M1-M7 顺序动手 task-30 实施。
