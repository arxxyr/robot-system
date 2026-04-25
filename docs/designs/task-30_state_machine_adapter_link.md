# Task-30 设计方案：state_machine ↔ adapter 执行链路

> **状态**：**v8**，等再次 review
> **历史**：v1 (`4cca734`) → v2 (`130b44b`) → v3 (`f7bdc00`) → v4 (`881e927`)
> → v5 (`46dd3c1`) → v6 (`4d016d4`) → v7 (`7334817`) → v8（本版本）
> **作者**：Claude Code
>
> **v8 关键修订**：v7 5 项 blocker 全收。最关键 #1：HardRT normal 提交协议
> 改 **单 packed `AtomicU64`**（action+arg+seq 一次 CAS），消除 v7 "先写 arg
> 再 cas" 的 arg 撕裂 + "cas 后 fetch_add seq" 的 seq 与 action 不匹配两个
> race；#2 SafetyStop 后**RT 端**二次拦截 pending normal Enable(true)；#3
> Busy 不能丢副作用请求 → ipc_thread 加 retry 协议；#4 trait 加
> `clear_safety_latch` 方法；#5 `sync_write_by_id_into` 改 `&mut [u8]` slice
> + 栈 `[u8; MAX_PARAMS]` middle buffer + 用 `register.length` 字段。

---

## §0 v7 → v8 修订记录

User v7 review 给出 5 项 blocker：

| # | v7 缺陷 | v8 修订 |
|---|---|---|
| **1** | 先 `store(arg)` 再 `cas(action_code)`：CAS 失败时 arg **已覆盖** 上一条 pending 的 arg；CAS 成功后 `fetch_add(seq)`：rt_thread 可能在 cas 完成 vs seq 完成之间 swap 出 action，**执行新 action 但记录旧 seq** | **§4.2 重写**：单个 `pending_normal: AtomicU64` packed slot（bits 0-7 action，8-15 arg，16-63 seq）；提交端 `seq_counter.fetch_add` 先分配 → packed → `compare_exchange(0, packed)` 占用；CAS 失败 → 该 seq 废弃（允许有空洞）；rt_thread `swap(0)` 一次性原子解码 action+arg+seq |
| **2** | RT loop 先消费 emergency 后**无条件**消费 normal —— 同一 tick 先断扭再执行已 pending 的 Enable(true) | **§4.2 + §4.4 重写 RT loop**：emergency 处理后**立即** `pending_normal.swap(0)` 丢弃 / 拒绝 in-flight normal；写一个 `RESP_ERR_PREEMPTED_BY_EMERGENCY` 让 client 看到 seq 失败；之后再处理新提交的 normal（仍要二次检查 latched） |
| **3** | v7 §12 Q2 "state_machine 收 Busy 直接放弃等下次 transition 重试"——transition 不一定再发生；**逻辑 Enabled 但硬件没 enable** | **§4.4 新增 ipc_thread retry 协议**：`pending_action: Option<(req, Option<seq>)>`，每个 tick 头部先处理 in-flight（poll seq → Ok 升进、Err 进 Fault；未 Submitted 则 retry request_normal）；只有 in-flight 完成才推进新 transition；副作用 transition (READY→ENABLED) **必须**等 poll 成功 |
| **4** | `safety_latched` 字段注释说 ClearEStop 清除，但 `ExecutorCtrlClient` trait 没暴露清 latch 方法 | **§4.1 trait 加** `fn clear_safety_latch(&self)`；HardRT impl `safety_latched.store(false)`；SerialRT impl 通过 IoState 字段；`state_machine` 处理 `Event::ClearEStop` 时**先**清持久化 latch + Fault，**最后**调 `clear_safety_latch` |
| **5** | `&mut Vec<u8> + push` 容量不足仍 alloc；用了不存在的字段 `reg.size`（实际是 `length`）；自己算 LEN 字段错（实际由 `build_instruction` 处理） | **§7.2 重写**：API 改 `sync_write_by_id_into(reg_name, entries, frame_buf: &mut [u8]) -> Result<(), FeetechError>`（slice，不是 Vec）；内部 middle params buffer 改栈 `[(u8, [u8; 4]); MAX_JOINTS]` + `[(u8, &[u8]); MAX_JOINTS]`（与 `build_write` 已有 `[u8; MAX_PARAMS]` 栈数组模式一致）；`build_sync_write` 内部 `vec!` → 栈数组（M4 内部修复）；用 `register.length`（事实字段名）；LEN/wire format 直接复用 `build_sync_write` 输出 |

v1-v7 文档归档 git history。v8 是当前权威。

---

## §1 问题陈述（v4-v7 保留）

略，详 v7 §1。三类 adapter / 三条 executor：HardRT (rt_thread) / SerialRT
(serial_io_thread) / SoftRT (io_thread, v0.4)；判定基准 = 可证明 RT 合规性 +
是否产生运动输出。

---

## §2 设计原则（v7 20 条 + v8 加 2）

1-20：v7 全保留。

**21（v8 新加）**：**RT 控制提交 = 单 packed atomic slot + CAS 占用**。多字段
分散 atomic（action / arg / seq 三个 store）会造成 ABA 与 race；必须单 atomic
（u32 / u64）pack 后 CAS。`store + fetch_add` 模式禁用。

**22（v8 新加）**：**emergency 必须抢占 in-flight normal**。RT loop 处理完
emergency 后**同一 tick** 立刻 drain pending normal slot，写 PreemptedByEmergency
result code，让 client 看到失败。普通逻辑严格保持 "emergency > pending normal
> 新 normal > policy gate write_command" 顺序。

**23（v8 新加）**：**带副作用的 transition 不可丢**。state_machine 对 Busy 必须
重试到 Submitted；对 Submitted 必须 poll 到 Ok / Err；中间未完成时禁止推进新
transition。READY → ENABLED 这种"逻辑切到硬件"的转换必须等硬件 ack 才算完成。

---

## §3 架构图（v7 保留 + v8 修订 normal slot 协议 + ipc retry）

```
┌─── robot-s0 ───────────────────────────────────────────────────────────┐
│                                                                        │
│  ipc_thread (state_machine + retry coordinator)                        │
│  ┌────────────────────────────────────────────────────────────────┐    │
│  │ each ipc tick:                                                 │    │
│  │ 1. 处理 in-flight pending_action（v8 §4.4）：                   │    │
│  │    - 已 Submitted{seq} → poll(seq) → Ok 升进 / Err 进 Fault     │    │
│  │    - 未 Submitted（上次 Busy）→ 重试 request_normal              │    │
│  │ 2. 处理 iceoryx2 events / state transition                     │    │
│  │ 3. transition 触发 set_policy + 排队新 pending_action（如果当前 │    │
│  │    无 in-flight）                                                │    │
│  │ executor_ctrl: Box<dyn ExecutorCtrlClient>                     │    │
│  │   trait 5 method: set_policy / safety_stop / request_normal /  │    │
│  │     poll_last_normal_resp(seq) / clear_safety_latch             │    │
│  └────────────────────────────────────────────────────────────────┘    │
│                                                                        │
│  ╔════════════════════════════════════════════════════════════════════╗│
│  ║  HardRtCtrlClient impl (单 packed AtomicU64)                       ║│
│  ║  ┌──────────────────────────────────────────────┐                  ║│
│  ║  │ policy:           AtomicU8                    │                  ║│
│  ║  │ emergency_action: AtomicU8                    │                  ║│
│  ║  │ pending_normal:   AtomicU64 ← packed (a/r/s)  │                  ║│
│  ║  │ seq_counter:      AtomicU64                   │                  ║│
│  ║  │ last_completed:   AtomicU64 ← packed (s/code) │                  ║│
│  ║  │ safety_latched:   AtomicBool                  │                  ║│
│  ║  └─────────────┬────────────────────────────────┘                  ║│
│  ║                ▼                                                   ║│
│  ║  rt_thread tick @ 50Hz (v8 重排)：                                ║│
│  ║   1. emergency_action.swap → safety_stop_rt → latch=true           ║│
│  ║      → drain pending_normal.swap(0) → write Preempted result       ║│
│  ║   2. pending_normal.swap(0) → 二次 latch 检查 → enable_rt /         ║│
│  ║      set_mode_rt → store last_completed (seq, code)                ║│
│  ║   3. policy gate (ActiveForward + !latched → write_command)        ║│
│  ║   4. read_state + publish snapshot                                 ║│
│  ║                                                                    ║│
│  ║  SerialRT 路径 v7 保留（emergency channel + pending normal ArcSwap）║│
│  ╚════════════════════════════════════════════════════════════════════╝│
└────────────────────────────────────────────────────────────────────────┘
```

---

## §4 ExecutorCtrlClient trait（v8 重写：5 method + packed atomic）

### 4.1 trait 定义（v8 加 `clear_safety_latch`）

```rust
pub trait ExecutorCtrlClient: Send + Sync {
    /// 跨 RtClass 统一门控；HardRT / SerialRT 都用 AtomicU8 store。
    fn set_policy(&self, policy: OutputPolicy);

    /// fire-and-forget；HardRT 写 emergency_action + drain pending normal；
    /// SerialRT 走 emergency channel + AtomicBool 兜底。
    fn safety_stop(&self);

    /// **HardRT**：CAS 占用 packed slot；失败返 `Busy`（**不**覆盖 pending）。
    /// **SerialRT**：写 ArcSwap pending；同名请求覆盖（普通线程允许）。
    fn request_normal(&self, req: AdapterNormalReq) -> NormalReqStatus;

    /// 拉指定 seq 的执行结果。`expected_seq` 不匹配返 `None`（旧 result 或还
    /// 没执行完）。
    fn poll_last_normal_resp(&self, expected_seq: u64) -> Option<AdapterResp>;

    /// **v8 新增**：清 RT 端 safety_latched。**仅** 在以下顺序调用：
    /// 1. `state_machine` 处理 `Event::ClearEStop`，校验权限（maintainer+ RPC）
    /// 2. EStop latch 持久化 `clear_estop_latch(...)` 成功
    /// 3. `S0State::Fault(...)` 通过 `Event::ClearError` 已转出
    /// 4. **最后**调 `clear_safety_latch`
    /// 顺序错 → 安全语义降级。文档 + 单测保证调用顺序。
    fn clear_safety_latch(&self);
}

#[derive(Clone, Copy, Debug)]
pub enum NormalReqStatus {
    Submitted { seq: u64 },         // CAS 占用成功；调用方持 seq 用于 poll
    Busy,                           // packed slot 已被占；调用方应**重试**（v8 §4.4）
    Unsupported(&'static str),      // 编码失败（HardRT 路径不支持的请求）
}
```

### 4.2 HardRT 实现：单 packed AtomicU64 slot

```rust
/// HardRT executor 的 ctrl client（**全 atomic**，rt_thread 零 alloc / 零 Arc refcount）。
pub struct HardRtCtrlInner {
    policy: AtomicU8,

    /// 紧急 action：1 = SafetyStop。rt_thread 优先消费。
    emergency_action: AtomicU8,

    /// **v8 packed slot**（替代 v7 三 atomic）：
    /// - bits  0..  7 = action_code (u8)：NONE=0 / ENABLE=1 / SET_MODE_POSITION=2
    /// - bits  8.. 15 = arg (u8)：Enable 用 0/1
    /// - bits 16.. 63 = seq (u48)：与提交端 `seq_counter` 同步
    /// 整 u64 = 0 → 无 pending；非 0 → action_code 必非零。
    /// 提交端：`compare_exchange(0, packed)`；rt_thread：`swap(0)`。
    pending_normal: AtomicU64,

    /// 全局 seq 计数器；每次 `request_normal` `fetch_add(1)` 分配；CAS 失败时
    /// 该 seq 废弃（允许空洞，简化 race 处理）。
    seq_counter: AtomicU64,

    /// rt_thread 完成后写：bits 0..47 = seq，bits 48..55 = result_code (u8)
    /// （reservoir 8 bits 给将来扩展用）。整 u64 = 0 → 无完成结果。
    last_completed: AtomicU64,

    /// EStop / Fault 后置位；rt_thread 拒绝 normal Enable(true)；`clear_safety_
    /// latch()` 清。
    safety_latched: AtomicBool,
}

#[inline]
fn pack_normal(action: u8, arg: u8, seq: u64) -> u64 {
    debug_assert!(action != 0); // 0 表示 NONE，不应 pack 进去
    debug_assert!(seq < (1u64 << 48));
    (action as u64) | ((arg as u64) << 8) | (seq << 16)
}

#[inline]
fn unpack_normal(packed: u64) -> (u8, u8, u64) {
    let action = (packed & 0xFF) as u8;
    let arg = ((packed >> 8) & 0xFF) as u8;
    let seq = packed >> 16;
    (action, arg, seq)
}

#[inline]
fn pack_completed(seq: u64, code: u8) -> u64 {
    (seq & ((1u64 << 48) - 1)) | ((code as u64) << 48)
}

#[inline]
fn unpack_completed(packed: u64) -> (u64, u8) {
    let seq = packed & ((1u64 << 48) - 1);
    let code = (packed >> 48) as u8;
    (seq, code)
}

impl ExecutorCtrlClient for HardRtCtrlClient {
    fn set_policy(&self, p: OutputPolicy) {
        self.inner.policy.store(p as u8, Ordering::Release);
    }

    fn safety_stop(&self) {
        self.inner.emergency_action.store(EMERGENCY_SAFETY_STOP, Ordering::AcqRel);
    }

    fn request_normal(&self, req: AdapterNormalReq) -> NormalReqStatus {
        // HardRT 编码集仅 RtSafeCtrl 方法（v7 §0 #2）
        let (action_code, arg) = match req {
            AdapterNormalReq::Enable(true) => (NORMAL_ENABLE, 1u8),
            AdapterNormalReq::Enable(false) => (NORMAL_ENABLE, 0u8),
            AdapterNormalReq::SetMode(ControlMode::Position) => (NORMAL_SET_MODE_POSITION, 0),
            AdapterNormalReq::Connect | AdapterNormalReq::Disconnect => {
                return NormalReqStatus::Unsupported(
                    "HardRT: Connect/Disconnect must be called from s0_daemon main"
                );
            }
            AdapterNormalReq::HoldCurrentPosition => {
                return NormalReqStatus::Unsupported(
                    "HardRT: HoldCurrent realized via OutputPolicy gate"
                );
            }
            AdapterNormalReq::SetMode(_) => {
                return NormalReqStatus::Unsupported(
                    "HardRT v0.2: only ControlMode::Position supported"
                );
            }
        };

        // safety_latched 时拒 Enable(true)（client 端早拒）
        if action_code == NORMAL_ENABLE && arg == 1
            && self.inner.safety_latched.load(Ordering::Acquire)
        {
            return NormalReqStatus::Unsupported(
                "Enable(true) refused: safety latched (call clear_safety_latch first)"
            );
        }

        // **v8 关键**：先分配 seq（CAS 失败时废弃，允许空洞）
        let new_seq = self.inner.seq_counter.fetch_add(1, Ordering::AcqRel) + 1;
        let packed = pack_normal(action_code, arg, new_seq);

        // 单次 CAS 占用 slot
        match self.inner.pending_normal.compare_exchange(
            0,
            packed,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => NormalReqStatus::Submitted { seq: new_seq },
            Err(_) => NormalReqStatus::Busy, // 上一条 normal 还没 swap 出去
        }
    }

    fn poll_last_normal_resp(&self, expected_seq: u64) -> Option<AdapterResp> {
        let packed = self.inner.last_completed.load(Ordering::Acquire);
        if packed == 0 {
            return None;
        }
        let (seq, code) = unpack_completed(packed);
        if seq != expected_seq {
            return None; // 旧 result 或新 seq 还没执行完
        }
        Some(decode_resp(code))
    }

    fn clear_safety_latch(&self) {
        self.inner.safety_latched.store(false, Ordering::Release);
    }
}
```

### 4.3 rt_thread 主循环（v8 重排：emergency drain pending）

```rust
fn run_rt_loop<A: ManipulatorAdapter + RtSafeCtrl>(
    cfg: RtThreadConfig,
    mut adapter: A, // 已 connect（s0_daemon main 在 spawn 前 connect）
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
                let s = match result {
                    Ok(()) => OUTPUT_SAFETY_TORQUE_OFF_CONFIRMED,
                    Err(_) => OUTPUT_SAFETY_STOP_FAILED,
                };
                safety_state_slot.store(s, Ordering::Release);

                // **v8 关键 (§0 #2)**：emergency 后立刻 drain pending normal
                // 防同一 tick 先断扭再 enable
                let dropped = ctrl.pending_normal.swap(0, Ordering::AcqRel);
                if dropped != 0 {
                    let (_action, _arg, seq) = unpack_normal(dropped);
                    // 写 Preempted result 让 client poll 看到失败
                    let pkt = pack_completed(seq, RESP_ERR_PREEMPTED_BY_EMERGENCY);
                    ctrl.last_completed.store(pkt, Ordering::Release);
                }
            }

            // 2. normal action（只要 emergency 已 drain，这里 swap 出来的必是
            //    emergency 之后**新提交**的；仍然二次 check latched 防 race）
            let packed = ctrl.pending_normal.swap(0, Ordering::AcqRel);
            if packed != 0 {
                let (action, arg, seq) = unpack_normal(packed);
                // **v8 二次 latch 检查**（§0 #2 + §2.22）
                let latched = ctrl.safety_latched.load(Ordering::Acquire);
                if action == NORMAL_ENABLE && arg == 1 && latched {
                    let pkt = pack_completed(seq, RESP_ERR_REJECTED_BY_LATCH);
                    ctrl.last_completed.store(pkt, Ordering::Release);
                } else {
                    let result = match action {
                        NORMAL_ENABLE => adapter.enable_rt(arg == 1),
                        NORMAL_SET_MODE_POSITION => adapter.set_mode_rt(ControlMode::Position),
                        _ => Err(RobotError::Unsupported("rt_thread: unknown normal_action")),
                    };
                    let code = encode_resp(&result);
                    let pkt = pack_completed(seq, code);
                    ctrl.last_completed.store(pkt, Ordering::Release);
                    // 注意：不在 RT 自动清 latch；ClearEStop 走 clear_safety_latch trait method
                }
            }

            // 3. policy gate
            let policy_u8 = ctrl.policy.load(Ordering::Acquire);
            let policy = OutputPolicy::from_u8(policy_u8);
            let latched = ctrl.safety_latched.load(Ordering::Acquire);
            if matches!(policy, OutputPolicy::ActiveForward) && !latched {
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

**新增 result code（v8）**：

```rust
pub const RESP_OK: u8 = 0;
pub const RESP_ERR_INVALID_PARAM: u8 = 1;
pub const RESP_ERR_HARDWARE_FAULT: u8 = 2;
pub const RESP_ERR_OTHER: u8 = 3;
pub const RESP_ERR_REJECTED_BY_LATCH: u8 = 4;       // v8 加：safety_latched 时 Enable(true) 拒
pub const RESP_ERR_PREEMPTED_BY_EMERGENCY: u8 = 5;  // v8 加：emergency 抢占 pending normal
```

### 4.4 ipc_thread retry 协议（v8 新增 §4.4）

state_machine 对 副作用 transition（READY → ENABLED.Active 等）**必须**保证
硬件状态对齐 —— 不能因为 `Busy` 一次就丢失意图。

```rust
// ipc_thread 持有的 state（伪代码）
pub struct IpcRetryState {
    /// 当前 in-flight normal action：
    /// - `Some((req, None))`：上次 request_normal 返 Busy，等下个 tick 重试
    /// - `Some((req, Some(seq)))`：已 Submitted{seq}，等 poll 返 result
    /// - `None`：无 in-flight，可以推进新 transition
    pending: Option<(AdapterNormalReq, Option<u64>)>,
}

fn ipc_tick(
    retry: &mut IpcRetryState,
    state_machine: &mut S0StateMachine,
    executor_ctrl: &dyn ExecutorCtrlClient,
    events: &mut EventQueue,
) {
    // 1. 推进 in-flight pending（**优先级最高**；未完成不开新 transition）
    if let Some((req, seq_opt)) = &retry.pending {
        match seq_opt {
            None => {
                // 上次 Busy，重试
                match executor_ctrl.request_normal(req.clone()) {
                    NormalReqStatus::Submitted { seq } => {
                        retry.pending = Some((req.clone(), Some(seq)));
                    }
                    NormalReqStatus::Busy => {
                        // 继续等下 tick；不推进 transition
                        return;
                    }
                    NormalReqStatus::Unsupported(msg) => {
                        // 永久失败 → 进 Fault
                        events.push(Event::AdapterErr(FaultCode::AdapterErr));
                        retry.pending = None;
                    }
                }
            }
            Some(seq) => {
                // 已 Submitted，等 poll
                match executor_ctrl.poll_last_normal_resp(*seq) {
                    None => return, // 还没执行完
                    Some(AdapterResp::Ok) => {
                        retry.pending = None; // 完成，可推进新 transition
                    }
                    Some(AdapterResp::Err(e)) => {
                        events.push(Event::AdapterErr(map_err_to_fault(e)));
                        retry.pending = None;
                    }
                }
            }
        }
    }

    // 2. 处理外部事件 + state_machine transition（**仅** in-flight 完成后）
    if retry.pending.is_none() {
        let trans = state_machine.try_transition(...);
        if let Some((old, new)) = trans {
            // 副作用 transition → 排队 pending
            if let Some(req) = action_for_transition(old, new) {
                retry.pending = Some((req, None)); // 下 tick 头部 request_normal
            }
            // 同时 set_policy（atomic store，立即生效，不入 retry queue）
            executor_ctrl.set_policy(OutputPolicy::from_state(new));
            // emergency safety_stop 也立即（fire-and-forget，不入 retry）
            if needs_safety_stop(old, new) {
                executor_ctrl.safety_stop();
            }
        }
    }
}
```

**关键约束**：
- `pending` 不为 `None` 时**不开新 transition** —— 否则丢副作用
- `set_policy` / `safety_stop` 是 atomic / fire-and-forget，**不入** retry queue（语义上"广播状态" + "广播紧急意图"，不等响应）
- `request_normal` Busy → 下 tick 重试 —— ipc tick 周期默认 500 µs（`ipc_thread.rs::IpcThreadConfig`），即使连续 Busy 也几个 tick 内能 Submit
- 长时间 in-flight 不完成（例如 rt_thread hang）→ ipc 加超时（如 1s）→ 升 Fault，避免永久卡

### 4.5 SerialRT 实现（v7 保留 + v8 加 `clear_safety_latch`）

`SerialIoCtrlClient` 用 emergency channel + AtomicBool 兜底 + ArcSwap pending
normal —— 普通线程允许 ArcSwap。详 v7 §4.3 + v5 §4.3。**v8 加** `clear_safety_
latch` 实现：通过 IoState `safety_latched: bool` 字段，trait 方法发 normal channel
`AdapterNormalReq::ClearLatch`（新增 variant）让 serial_io_thread 处理。

---

## §5 OutputPolicy / OutputSafetyState / OutputSafetyProbe / RtSafeCtrl

详 v7 §5（保留），含 v6/v7 修订：6 OutputPolicy + AdapterUncertain=5 +
OutputSafetyProbe trait + RtSafeCtrl 子 trait。

---

## §6 SerialRT confirm_torque_off 路径（v6/v7 保留）

详 v7 §6。`StopCommanded → confirm → all_off / any_off / none_off` 三分支。

---

## §7 Feetech `safety_stop_fast` + 预分配 frame API（v8 修 #5）

### 7.1 当前状态

`enable(false)` 13 motor × 3 retry × 150ms = ~5.85s。

### 7.2 v8 实现：用现有 `build_sync_write` slice API + 内部栈数组

**v8 关键认知**：现有 `protocol.rs:170` `build_sync_write(addr, length, per_motor,
out: &mut [u8]) -> Result<usize, FeetechError>` **已经是 slice-based**！v7 文档
里我**误描述**为"Vec-based"。但其内部 `let mut params = vec![0u8; total_params]`
**仍 alloc** middle params buffer。

v8 修订（实际改动小）：

**(a) `protocol::build_sync_write` 内部 vec → 栈数组**（与现有 `build_write`
`[u8; MAX_PARAMS]` 模式一致）：

```rust
pub fn build_sync_write(
    addr: u8,
    length: u8,
    per_motor: &[(u8, &[u8])],
    out: &mut [u8],
) -> Result<usize, FeetechError> {
    // ... validation 不变 ...

    let entry_size = 1 + length_usize;
    let total_params = 2 + per_motor.len() * entry_size;
    if total_params > MAX_PARAMS {
        return Err(FeetechError::BadPacketLength { len: 0, body_len: total_params });
    }

    // **v8**：栈数组替代 vec![0u8; total_params]
    let mut params = [0u8; MAX_PARAMS];
    params[0] = addr;
    params[1] = length;
    for (i, (id, data)) in per_motor.iter().enumerate() {
        let off = 2 + i * entry_size;
        params[off] = *id;
        params[off + 1..off + 1 + length_usize].copy_from_slice(data);
    }
    build_instruction(BROADCAST_ID, Instruction::SyncWrite, &params[..total_params], out)
}
```

**(b) `bus::sync_write_by_id_into` 新增 slice-based API**（与 `sync_read_by_id_into`
对称；不阻塞已有 `sync_write_by_id` 的调用方）：

```rust
impl<T: SerialTransport> FeetechBus<T> {
    /// 预分配 frame buffer 版本。**`frame_buf` 必须 `>= MAX_FRAME_LEN`**
    /// （258 = 4 header + 254 max payload + 1 checksum，宽容 256 → 取 256 即可）；
    /// 不足 → `BufferTooSmall`。运行期不 alloc。
    pub fn sync_write_by_id_into(
        &mut self,
        reg_name: &'static str,
        entries: &[(u8, i32)],
        frame_buf: &mut [u8],
    ) -> Result<(), FeetechError> {
        let reg = get_register(reg_name)?;
        let length_usize = usize::from(reg.length); // **v8 fix**：字段名是 length 不是 size
        let n = entries.len();
        if n > robot_core::adapter::MAX_JOINTS {
            return Err(FeetechError::ByteLengthMismatch {
                expected: robot_core::adapter::MAX_JOINTS,
                actual: n,
            });
        }

        // 栈数组：每 motor 4 byte data buffer（u32 max）
        let mut data_bufs: [[u8; 4]; MAX_JOINTS] = [[0; 4]; MAX_JOINTS];
        let mut entry_refs: [(u8, &[u8]); MAX_JOINTS] = [(0, &[]); MAX_JOINTS]; // 占位

        // 编码 + 切 slice
        for (i, (id, value)) in entries.iter().enumerate() {
            let encoded = encode_value(reg_name, *value)?;
            split_bytes_le(encoded, length_usize, &mut data_bufs[i][..length_usize])?;
            entry_refs[i] = (*id, &data_bufs[i][..length_usize]);
        }

        let n_bytes = build_sync_write(reg.addr, reg.length, &entry_refs[..n], frame_buf)?;
        self.transport.write_all(&frame_buf[..n_bytes])
    }
}
```

**(c) `FeetechAdapter::safety_stop_fast` 用 slice frame_buf**：

```rust
pub struct FeetechAdapter<T: SerialTransport> {
    // ... 现有字段 ...
    /// v8 预分配 (id, value=0) buffer：N joints × (id, 0)
    safety_stop_entries: Vec<(u8, i32)>,
    /// v8 预分配 wire frame buffer（用 fixed 256 byte 或 Vec 仅启动 alloc）
    safety_stop_frame_buf: Vec<u8>,
    /// confirm_torque_off raw 缓冲
    confirm_torque_buf: Vec<i32>,
}

impl FeetechAdapter {
    pub fn safety_stop_fast(&mut self) -> Result<(), FeetechError> {
        // safety_stop_entries 在 connect 时已填 (id, 0)
        // 这里不重新 alloc 也不修改值（一直 0）
        self.bus.sync_write_by_id_into(
            "Torque_Enable",
            &self.safety_stop_entries,
            &mut self.safety_stop_frame_buf, // capacity 已 reserve(256)
        )
    }

    pub fn connect(&mut self) -> Result<(), RobotError> {
        // ... 现有 handshake + joint_id 缓存 ...
        // v8 预分配
        self.safety_stop_entries.clear();
        for &id in &self.joint_ids {
            self.safety_stop_entries.push((id, 0));
        }
        self.safety_stop_frame_buf.clear();
        self.safety_stop_frame_buf.resize(256, 0); // 一次性预填 256 byte
        self.confirm_torque_buf.resize(self.joint_ids.len(), 0);
        Ok(())
    }
}
```

**(d) M4 测试**：
- `assert_no_alloc::assert_no_alloc(|| adapter.safety_stop_fast()).unwrap()`
  不 panic
- mock transport：wire bytes 含 broadcast id `0xFE` + `Torque_Enable.addr=40` +
  全 0 data
- 时延 ≤ 5ms（mock 即时）

---

## §8 SIGTERM graceful + SIGKILL blocking gap（v5 保留）

详 v5 §9。

---

## §9 测试覆盖（v7 + v8）

继承 v7 全部 + v8 加：

| 用例 | 验证 |
|---|---|
| `pack_normal` / `unpack_normal` 全 valid 范围 round-trip | v8 #1 |
| `request_normal` 两次连续 → 第一返 `Submitted{seq=N}`，第二返 `Busy`（**不**覆盖 packed slot；不消耗 seq counter） | v8 #1 + R6 |
| rt_thread `swap` 拿到 packed → action+arg+seq 一致；写 `last_completed` 用同一 seq | v8 #1 |
| `request_normal(Enable(true))` 后立刻 `safety_stop` → rt_thread 一个 tick 内：emergency drain pending 写 `RESP_ERR_PREEMPTED_BY_EMERGENCY`；`Enable(true)` 不被执行 | v8 #2 |
| safety_stop 处理后**新提交**的 Enable(true)（client 端没看到 latch 已置位的瞬间）→ rt_thread 二次检查 latched → 写 `RESP_ERR_REJECTED_BY_LATCH` | v8 #2 |
| ipc retry 协议：`request_normal` 返 Busy → 下 tick 重试 → 最终 Submitted；poll Ok → pending 清 → 推进新 transition | v8 #3 |
| in-flight pending 期间收到新 transition → 不立即排新 pending，等当前完成 | v8 #3 |
| in-flight pending 超过 1s → 自动升 Fault（避免永久卡） | v8 #3 |
| `executor_ctrl.clear_safety_latch()` HardRT 把 `safety_latched` 设 false；先于此调用的 Enable(true) request 仍被拒 | v8 #4 |
| `state_machine` `Event::ClearEStop` 处理顺序：清 EStop latch → 清 Fault → `clear_safety_latch` → 才允许 Enable(true) | v8 #4 |
| `sync_write_by_id_into` 在 `assert_no_alloc::assert_no_alloc(...)` scope 不 panic（`build_sync_write` 内部栈数组 + bus 入口栈数组） | v8 #5 |
| `sync_write_by_id_into` wire bytes 与 dimos 期望对照（13 motor + 1 byte reg + broadcast id） | v8 #5 |
| `register.length` 字段引用（不是 `size`）—— 单测覆盖 build_sync_write 各寄存器 | v8 #5 |
| **【blocking gap】** SIGKILL → torque off **不通过**（标 known issue，留 task-30-post） | 不假装 |

---

## §10 风险（v7 + v8 加）

### v8 R16：`request_normal` Busy 死循环

**风险**：rt_thread hang 或 ipc tick 远快于 rt tick → ipc 重试 Busy 永远循环。

**缓解**：
- ipc retry 协议加 **总超时 1s**（`pending` 持续 > 1s 未完成 → Fault）
- rt_thread 已有 `assert_no_alloc + rt_guard_soak`（task 26）保证 hang 不会发生
- 实测 ipc tick = 500 µs，rt tick = 20 ms → ipc 在一个 rt tick 内最多 40 次 Busy
  尝试，1s 总预算够 50 个 rt tick，远超合理范围

### v8 R17：seq_counter wrap-around (u48)

**风险**：u48 seq = 281T，50Hz tick 一年发 50×3600×24×365 = 1.6G 次，**∼175 年
才 wrap**。但 SerialRT 内部 seq 也是 u64 同处理。

**缓解**：可接受；v0.x 周期不会触发；wrap 时 last_completed 等也会 wrap，逻辑
仍正确。

---

## §11 迁移计划

### 11.1 前置 task-30-pre（v7 保留）

详 v7 §11.1。pre-1a/b/c/d + pre-2 + pre-3a/b。

### 11.2 主体 M1-M7（v8 改 M4 + M5）

| # | 主题 | 工作量 | 依赖 |
|---|---|---|---|
| **M1** | `ExecutorCtrlClient` trait（**5 method** 含 clear_safety_latch）+ `OutputPolicy` AtomicU8 + `OutputSafetyState` (含 StopCommanded + AdapterUncertain) + `AdapterNormalReq` + `NormalReqStatus` + atomic 编码常量（含 `RESP_ERR_REJECTED_BY_LATCH` / `RESP_ERR_PREEMPTED_BY_EMERGENCY`）；`pack_normal` / `unpack_normal` / `pack_completed` 单测 | 1 h | pre-3a/b |
| **M2** | `RtSafeCtrl` 子 trait + `MockAdapter` 实现 + `assert_no_alloc` 单测；`ManipulatorAdapter::write_command(mode=Stop)` 合约 | 1 h | M1 |
| **M3** | `OutputSafetyProbe` trait + `FeetechAdapter` 实现 + `MockAdapter` 实现 | 1 h | pre-1 |
| **M4** | **v8 重写**：`protocol::build_sync_write` 内部 vec → 栈数组；`bus::sync_write_by_id_into` 新增 slice API；`FeetechAdapter::safety_stop_fast()` 走新 API + 预分配；`assert_no_alloc` 单测 | 1.5 h | M3 |
| **M5** | **v8 重写**：`HardRtCtrlClient` (单 packed AtomicU64) + rt_thread 主循环 (emergency drain pending + 二次 latch check + clear_safety_latch via trait method) + `assert_no_alloc` soak | 2 h | M1 / M2 |
| **M6** | `SerialIoCtrlClient` + `serial_io_thread` skeleton + confirm_torque_off 路径 (StopCommanded → 三分支) + clear_safety_latch SerialRT impl；feature flag off | 2.5 h | M1 / M3 / M4 / pre-3b |
| **M7** | `s0_daemon` dispatch by RtClass；ipc_thread 加 `IpcRetryState` + retry 协议 + transition hook (set_policy 立即 / safety_stop 立即 / pending normal 排队 retry)；删 legacy bootstrap；feature flag default on；smoke 通过下个 release 删 flag | 2 h | M5 / M6 / pre-2 |

**总 ~11 h**（v7 估 10h；v8 加 packed atomic + ipc retry + clear_safety_latch +
emergency drain +0.5h × 2 = +1h）。

---

## §12 v8 决议请求

请 review 并回复：

1. v8 整体（packed atomic slot + emergency drain pending + ipc retry 协议 +
   clear_safety_latch trait method + sync_write_by_id_into slice API）approve？
2. **`pending_normal: AtomicU64` packed (action 8 / arg 8 / seq 48)** 设计是否合适？要不要预留更多 reserved bits？
3. **emergency 后 RT 立即 drain pending normal**（写 `RESP_ERR_PREEMPTED_BY_EMERGENCY`）是否同意？还是只 reject Enable(true)、其他 normal action 保留？
4. **ipc_thread retry 协议**（in-flight 不完成不开新 transition；超时 1s 升 Fault）是否同意？
5. **`clear_safety_latch` trait method** 调用顺序约定（latch 文件清 → Fault 清 → clear_safety_latch）是否需要在 trait doc 强制 / 有运行时校验？
6. M4 改动 (protocol 内栈数组 + bus.sync_write_by_id_into slice API) 是否影响现有 `sync_write_by_id` 调用方？保留 vs 删除现有 API？
7. M7 拆两 commit（"flag on" + "删 legacy"）？
8. 真机最低门槛 M7 smoke：v7 + v8 加 `request_normal` Busy retry 测 / emergency drain pending 验证（mock 提交 Enable(true) 后 safety_stop → poll 看到 Preempted） / `clear_safety_latch` 顺序违反时拒 enable（mock）。同意 / 加项？

review 通过我按 pre-1 → pre-2 → pre-3 → M1-M7 顺序动手 task-30 实施。
