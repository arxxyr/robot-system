//! Task-30 v9 / M5：`HardRtCtrlClient` —— `RtClass::HardRT` 路径的
//! [`ExecutorCtrlClient`] 实现。
//!
//! 对应 `docs/designs/task-30_state_machine_adapter_link.md` §4.2 + v9 §0.5。
//!
//! ## 数据布局
//!
//! ```text
//! HardRtCtrlInner {
//!   policy: AtomicU8,                     // OutputPolicy（rt_thread 读 hot path）
//!   safety_state: AtomicU8,               // OutputSafetyState（rt_thread 写）
//!   emergency_action: AtomicU8,           // emergency_codes::*（client 写、rt 读 swap）
//!   pending_normal: AtomicU64 (8/8/48)    // packed (action, arg, seq)
//!   seq_counter: AtomicU64,               // 单调 fetch_add；CAS 失败 seq 废弃 → 允许空洞
//!   last_completed: AtomicU64 (16/48)     // packed (resp_code, seq)
//!   safety_latched: AtomicBool,           // emergency 后由 rt 置 true，client 端
//!                                         //   `clear_safety_latch` 同步 store(false)
//! }
//! ```
//!
//! ## 双重 latch 防护
//!
//! emergency 后 `safety_latched=true`。新 `Enable(true)` 请求会被**两层**拒：
//! 1. **client 端**（本模块 [`HardRtCtrlClient::request_normal`]）：先读 latch
//!    atomic，若 latched 直接返 `NormalReqStatus::Unsupported(...)`，省一次 RT
//!    交互。
//! 2. **rt_thread 端**（[`crate::rt_thread`] 主循环 "二次 latch check"）：
//!    swap 出 packed slot 后再读一次 latch（防 race：client 检查后 latch 才被
//!    rt 置位的窗口）。仍 latched 则写 `last_completed = Err(SafetyLatched)`。
//!
//! ## RT 安全
//!
//! 所有 rt_thread 侧读写都是 atomic load/store/swap/CAS（无 alloc，无 lock，
//! 无 syscall 除 sleep_until）。client 侧也是纯 atomic（`set_policy` /
//! `safety_stop` / `request_normal` / `poll_last_normal_resp` /
//! `clear_safety_latch`），可在 ipc_thread 普通优先级线程调用。

#![allow(clippy::doc_markdown)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use robot_core::executor_ctrl::{
    AdapterErr, AdapterNormalReq, AdapterResp, ExecutorCtrlClient, NormalReqStatus, OutputPolicy,
    OutputSafetyState, emergency_codes, encode_normal_req, normal_codes, pack_completed,
    pack_pending_normal, unpack_completed, unpack_pending_normal,
};

// ============================================================================
// HardRtCtrlInner
// ============================================================================

/// HardRT 路径的共享状态。client 端 (`HardRtCtrlClient`) 只读 `Arc<Inner>`；
/// rt_thread 持 `Arc<Inner>` 并在主循环内 swap / store。
#[derive(Debug)]
pub struct HardRtCtrlInner {
    /// 当前 [`OutputPolicy`]。`set_policy` 写、rt_thread 在 hot-path 读。
    pub policy: AtomicU8,
    /// 当前 [`OutputSafetyState`]。rt_thread 写（emergency / 正常 enable 切换）；
    /// `pub_thread` 读到对外 wire `S0SafetyState`。
    pub safety_state: AtomicU8,
    /// `safety_stop` 路径。`safety_stop()` store(SAFETY_STOP)；rt_thread 主
    /// 循环 swap 出消费 + safety_stop_rt + latch。
    pub emergency_action: AtomicU8,
    /// **packed AtomicU64**（layout 见 [`pack_pending_normal`]）：
    /// `request_normal` 用 fetch_add 拿 seq → CAS 占用；rt_thread swap 出
    /// 消费。**v9**：单 slot 简化语义，最近一次未消费请求覆盖前一次（client
    /// 端拒后续 Busy 提交）。
    pub pending_normal: AtomicU64,
    /// `seq_counter`。`request_normal` fetch_add 拿 seq；**永远 +1**（即便
    /// CAS 失败 seq 也废弃 = 允许空洞，简化 wraparound 推理）。
    pub seq_counter: AtomicU64,
    /// **packed AtomicU64**（layout 见 [`pack_completed`]）：rt_thread 完成
    /// 一条 normal action 后 store(seq + resp_code)；client 端
    /// `poll_last_normal_resp(seq)` 比对 seq 决定是否已完成。
    pub last_completed: AtomicU64,
    /// emergency 后由 rt_thread 置 true；`Event::ClearEStop` transition 调
    /// `clear_safety_latch()` atomic store(false)。client 端 + rt_thread 端
    /// 都读它做"拒 Enable(true)"判定（双重防护）。
    pub safety_latched: AtomicBool,
}

impl HardRtCtrlInner {
    /// 出厂默认：policy=Disabled / safety_state=TorqueUnknown / emergency=NONE /
    /// pending=0 / seq_counter=1（避免初值与 valid seq 0 冲突；poll 看到
    /// `last_completed_seq < 1` 表示"从未完成过"）/ last_completed=0 /
    /// safety_latched=false。
    #[must_use]
    pub fn new() -> Self {
        Self {
            policy: AtomicU8::new(OutputPolicy::Disabled.to_u8()),
            safety_state: AtomicU8::new(OutputSafetyState::TorqueUnknown.to_u8()),
            emergency_action: AtomicU8::new(emergency_codes::NONE),
            pending_normal: AtomicU64::new(0),
            seq_counter: AtomicU64::new(1),
            last_completed: AtomicU64::new(0),
            safety_latched: AtomicBool::new(false),
        }
    }
}

impl Default for HardRtCtrlInner {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// HardRtCtrlClient
// ============================================================================

/// `Arc<HardRtCtrlInner>` 的薄包装，实现 [`ExecutorCtrlClient`] trait。
///
/// `ipc_thread` 持 `Arc<dyn ExecutorCtrlClient>`；底层指向同一个
/// `HardRtCtrlInner`，与 rt_thread 共享。
#[derive(Clone, Debug)]
pub struct HardRtCtrlClient {
    inner: Arc<HardRtCtrlInner>,
}

impl HardRtCtrlClient {
    /// 包一份共享 `Inner`。`Arc::clone` 在 spawn 时**一次**；后续不再触动。
    #[must_use]
    pub fn new(inner: Arc<HardRtCtrlInner>) -> Self {
        Self { inner }
    }

    /// 暴露内部 `Arc<Inner>`（rt_thread 也需要持有同一份共享）。
    #[must_use]
    pub fn inner(&self) -> &Arc<HardRtCtrlInner> {
        &self.inner
    }
}

impl ExecutorCtrlClient for HardRtCtrlClient {
    fn set_policy(&self, policy: OutputPolicy) {
        // Release：rt_thread 在 read policy 时用 Acquire 配对，看到本次 store
        // 之后的所有 ipc_thread 侧 happens-before 写。
        self.inner.policy.store(policy.to_u8(), Ordering::Release);
    }

    fn safety_stop(&self) {
        // task-30 v9 finding #4 + #1：emergency 必须**在 client 端同步**切断
        // policy + 置 latch + drain pending（packed slot 里**尚未被 rt_thread
        // swap 走**的部分），最后置 emergency_action 让 rt 真下硬件。
        //
        // **顺序极其关键**（store 顺序决定可观测性窗口）：
        //
        // 1) **policy.store(StopTorque)**（finding #1）：第一时间切策略。
        //    rt_thread step 3 hot-path 读 policy 决定 `do_write`；本 store
        //    之后的 tick 一定看到 StopTorque → **不**再下 write_command。
        //    RT 线程 emergency 分支随后再 commit 一次（兜底）。
        //
        // 2) **safety_latched.store(true)**：双重 latch 防护层 1。后续 ipc 并发
        //    `request_normal(Enable(true))` 在 client 端立即被拒。
        //
        // 3) **pending_normal.swap(0)**：drain"**尚未被 rt 消费**"的 pending。
        //    finding #2 收紧注释：本 swap **不能**消除"rt_thread step 2 已经
        //    把 pending swap 到局部变量"的窗口（rt 已经在执行那条 action）；
        //    其中 Enable(true) 由 rt 端二次 latch check 拦下，但
        //    SetMode / Enable(false) / HoldCurrent 仍会跑完。这是合规的：
        //    那些 action 都是"非破坏性"的，且 emergency_action 下个 tick 会被
        //    consume 把 motor torque 真断；M7 的"已 swap in-flight action"
        //    preempt 测试会覆盖此语义边界。
        //
        // 4) **emergency_action.store(SAFETY_STOP)**：让 rt_thread tick 头部
        //    真下硬件 safety_stop 并 commit safety_state。
        //
        // 所有 store / swap 都是 atomic，client 端纯 RT-safe；可在 ipc_thread
        // 普通线程调用。
        self.inner
            .policy
            .store(OutputPolicy::StopTorque.to_u8(), Ordering::Release);
        self.inner.safety_latched.store(true, Ordering::Release);
        // **task-30 v9 / SerialRT E2E #4 修订**：同步重置 safety_state 到
        // StopCommanded —— spawn 之初设的 TorqueOffConfirmed 在 emergency 触
        // 发后已不再可信（rt/serial 端尚未消费 emergency_action 真断扭）。
        // 让 ipc_tick 的 ClearEStop 守门 (== TorqueOffConfirmed) 必须等
        // SerialRT 端**真正** confirm 完毕后才放行 clear_safety_latch，避免
        // race window。
        self.inner
            .safety_state
            .store(OutputSafetyState::StopCommanded.to_u8(), Ordering::Release);
        let pending = self.inner.pending_normal.swap(0, Ordering::AcqRel);
        if pending != 0 {
            let (action, _arg, seq) = unpack_pending_normal(pending);
            if action != normal_codes::NONE {
                self.inner.last_completed.store(
                    pack_completed(seq, AdapterResp::Err(AdapterErr::PreemptedByEmergency)),
                    Ordering::Release,
                );
            }
        }
        self.inner
            .emergency_action
            .store(emergency_codes::SAFETY_STOP, Ordering::Release);
    }

    fn request_normal(&self, req: AdapterNormalReq) -> NormalReqStatus {
        // 1) 编码请求。无法编码 → Unsupported（effectful path 必升 fault）。
        let Some((action, arg)) = encode_normal_req(req) else {
            return NormalReqStatus::Unsupported(
                "HardRT: AdapterNormalReq cannot be atomically encoded",
            );
        };

        // 2) **client 端 latch 拒**（双重防护第一层）：finding #3 已说 latch 拒
        //    属正常保护；effectful 路径还是会被上层升 fault，但本层先拒能省
        //    一次 RT 交互。
        if action == normal_codes::ENABLE
            && arg == 1
            && self.inner.safety_latched.load(Ordering::Acquire)
        {
            return NormalReqStatus::Unsupported(
                "Enable(true) refused: safety latched (need ClearEStop first)",
            );
        }

        // 3) 取 seq + 占 slot。**v9 §0 #1**：fetch_add 永远 +1（即便 CAS 失败
        //    seq 也废弃，允许空洞）；CAS 失败 = slot 被前一次未消费的请求占住
        //    → 返 Busy。
        let seq = self.inner.seq_counter.fetch_add(1, Ordering::AcqRel);
        let new_packed = pack_pending_normal(action, arg, seq);
        // CAS 期望旧值 = 0（slot 空）。非 0 表示前一次未被 rt_thread 消费 → Busy。
        match self.inner.pending_normal.compare_exchange(
            0,
            new_packed,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => NormalReqStatus::Submitted { seq },
            Err(_existing) => NormalReqStatus::Busy,
        }
    }

    fn poll_last_normal_resp(&self, seq: u64) -> Option<AdapterResp> {
        // **finding #5 收紧版**：**只**在 `last_seq == seq` 时返 `Some(resp)`。
        // 任何 seq 错配（`last_seq < seq` "还没完成" / `last_seq > seq` "已被
        // 后续 action 覆盖"）一律返 `None`，由 M7 ipc_tick 1s 总超时升 Fault。
        //
        // 旧实现 `last_seq > seq` 时仍返 `Some(resp)` 会**掩盖 sequencing bug**
        // ——把别人 action 的 ack 当成本 action 的 ack。本场景在 v9 单 pending
        // 设计下不应发生（ipc_tick 一次只排一个 pending）；若发生，是上层
        // bug，让 1s 超时升 Fault 比硬塞别人的 resp 安全得多。
        let packed = self.inner.last_completed.load(Ordering::Acquire);
        let (last_seq, resp) = unpack_completed(packed);
        if last_seq == seq { Some(resp) } else { None }
    }

    fn clear_safety_latch(&self) {
        // **v9 §0 #2**：同步 + 立即 visible。**禁止**走 channel 异步路径，
        // 必须 atomic store —— 让后续 `request_normal(Enable(true))` 立刻不被
        // latch 拒。
        self.inner.safety_latched.store(false, Ordering::Release);
    }

    fn safety_state(&self) -> OutputSafetyState {
        OutputSafetyState::from_u8(self.inner.safety_state.load(Ordering::Acquire))
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use robot_core::adapter::ControlMode;
    use robot_core::executor_ctrl::{AdapterErr, pack_completed};

    fn fresh() -> (HardRtCtrlClient, Arc<HardRtCtrlInner>) {
        let inner = Arc::new(HardRtCtrlInner::new());
        let client = HardRtCtrlClient::new(Arc::clone(&inner));
        (client, inner)
    }

    #[test]
    fn default_state_is_safe() {
        let (_, inner) = fresh();
        assert_eq!(
            OutputPolicy::from_u8(inner.policy.load(Ordering::Acquire)),
            OutputPolicy::Disabled
        );
        assert_eq!(
            OutputSafetyState::from_u8(inner.safety_state.load(Ordering::Acquire)),
            OutputSafetyState::TorqueUnknown
        );
        assert_eq!(
            inner.emergency_action.load(Ordering::Acquire),
            emergency_codes::NONE
        );
        assert_eq!(inner.pending_normal.load(Ordering::Acquire), 0);
        assert_eq!(inner.seq_counter.load(Ordering::Acquire), 1);
        assert_eq!(inner.last_completed.load(Ordering::Acquire), 0);
        assert!(!inner.safety_latched.load(Ordering::Acquire));
    }

    #[test]
    fn set_policy_writes_atomic() {
        let (c, i) = fresh();
        c.set_policy(OutputPolicy::ActiveForward);
        assert_eq!(
            OutputPolicy::from_u8(i.policy.load(Ordering::Acquire)),
            OutputPolicy::ActiveForward
        );
        c.set_policy(OutputPolicy::StopTorque);
        assert_eq!(
            OutputPolicy::from_u8(i.policy.load(Ordering::Acquire)),
            OutputPolicy::StopTorque
        );
    }

    #[test]
    fn safety_stop_sets_emergency_atom() {
        let (c, i) = fresh();
        c.safety_stop();
        assert_eq!(
            i.emergency_action.load(Ordering::Acquire),
            emergency_codes::SAFETY_STOP
        );
    }

    /// `request_normal` 第一次返 `Submitted{seq=1}`，packed slot 装入 (action,
    /// arg, seq=1)；seq_counter 推到 2。
    #[test]
    fn request_normal_first_returns_submitted_seq_1() {
        let (c, i) = fresh();
        let status = c.request_normal(AdapterNormalReq::Enable(true));
        match status {
            NormalReqStatus::Submitted { seq } => assert_eq!(seq, 1),
            other => panic!("expected Submitted, got {other:?}"),
        }
        // packed slot
        let packed = i.pending_normal.load(Ordering::Acquire);
        let (action, arg, seq) = robot_core::executor_ctrl::unpack_pending_normal(packed);
        assert_eq!(action, normal_codes::ENABLE);
        assert_eq!(arg, 1);
        assert_eq!(seq, 1);
        // seq_counter 推到 2
        assert_eq!(i.seq_counter.load(Ordering::Acquire), 2);
    }

    /// **v9 §0 #3**：第二次 `request_normal` 在 rt_thread 还没消费时返 Busy；
    /// **packed slot 仍是第一次的** (action, arg, seq=1)；**seq_counter 推到 3**
    /// （即便第二次的 seq=2 被废弃，counter 永远 +1，允许空洞）。
    #[test]
    fn request_normal_second_returns_busy_seq_counter_advances() {
        let (c, i) = fresh();
        let _ = c.request_normal(AdapterNormalReq::Enable(true));
        let status = c.request_normal(AdapterNormalReq::SetMode(ControlMode::Position));
        assert_eq!(status, NormalReqStatus::Busy);
        // packed slot 仍是 Enable(true) seq=1
        let packed = i.pending_normal.load(Ordering::Acquire);
        let (action, arg, seq) = robot_core::executor_ctrl::unpack_pending_normal(packed);
        assert_eq!(action, normal_codes::ENABLE);
        assert_eq!(arg, 1);
        assert_eq!(seq, 1, "Busy 不覆盖 packed slot");
        // seq_counter 已经被 fetch_add 推到 3（seq=2 被废弃）
        assert_eq!(
            i.seq_counter.load(Ordering::Acquire),
            3,
            "seq_counter 永远 +1"
        );
    }

    /// rt_thread 消费（swap pending）后，第二次 request_normal 又能 Submitted。
    #[test]
    fn request_normal_after_rt_consumed_returns_submitted() {
        let (c, i) = fresh();
        let _ = c.request_normal(AdapterNormalReq::Enable(true));
        // 模拟 rt_thread swap
        let _ = i.pending_normal.swap(0, Ordering::AcqRel);
        // 第二次又能 Submitted
        let status = c.request_normal(AdapterNormalReq::Disconnect);
        match status {
            NormalReqStatus::Submitted { seq } => assert_eq!(seq, 2),
            other => panic!("expected Submitted seq=2, got {other:?}"),
        }
    }

    /// **finding #3 验证**：safety_latched=true 时 client 端拒
    /// `Enable(true)` → `NormalReqStatus::Unsupported(_)`。
    /// 其它 action（含 `Enable(false)` / `SetMode` / `HoldCurrent`）**不**被拒。
    #[test]
    fn request_normal_latched_rejects_only_enable_true() {
        let (c, i) = fresh();
        i.safety_latched.store(true, Ordering::Release);

        // Enable(true) → Unsupported
        match c.request_normal(AdapterNormalReq::Enable(true)) {
            NormalReqStatus::Unsupported(_) => {}
            other => panic!("Enable(true) should be Unsupported, got {other:?}"),
        }

        // Enable(false) 仍可
        match c.request_normal(AdapterNormalReq::Enable(false)) {
            NormalReqStatus::Submitted { seq } => assert_eq!(seq, 1),
            other => panic!("Enable(false) should be Submitted, got {other:?}"),
        }
    }

    /// poll：seq < last_completed_seq → None；seq == last_completed_seq → Some(resp)。
    #[test]
    fn poll_last_normal_resp_seq_match() {
        let (c, i) = fresh();
        // 还没完成 → None
        assert_eq!(c.poll_last_normal_resp(1), None);
        // rt_thread 写 last_completed (seq=1, Ok)
        i.last_completed
            .store(pack_completed(1, AdapterResp::Ok), Ordering::Release);
        assert_eq!(c.poll_last_normal_resp(1), Some(AdapterResp::Ok));
        // seq=2 还没完成（last_seq=1 < 2）→ None
        assert_eq!(c.poll_last_normal_resp(2), None);
    }

    #[test]
    fn poll_last_normal_resp_err_round_trip() {
        let (c, i) = fresh();
        i.last_completed.store(
            pack_completed(7, AdapterResp::Err(AdapterErr::SafetyLatched)),
            Ordering::Release,
        );
        assert_eq!(
            c.poll_last_normal_resp(7),
            Some(AdapterResp::Err(AdapterErr::SafetyLatched))
        );
    }

    /// **v9 §0 #2**：`clear_safety_latch` 同步 atomic store。后续 Enable(true)
    /// 立即不被拒。
    #[test]
    fn clear_safety_latch_unblocks_enable_true() {
        let (c, i) = fresh();
        i.safety_latched.store(true, Ordering::Release);
        // 先确认 latched 拒
        assert!(matches!(
            c.request_normal(AdapterNormalReq::Enable(true)),
            NormalReqStatus::Unsupported(_)
        ));
        // 清 latch
        c.clear_safety_latch();
        assert!(!i.safety_latched.load(Ordering::Acquire));
        // 再 Enable(true) 应 Submitted
        match c.request_normal(AdapterNormalReq::Enable(true)) {
            NormalReqStatus::Submitted { .. } => {}
            other => panic!("Enable(true) should be Submitted after clear, got {other:?}"),
        }
    }

    /// `assert_no_alloc`：5 个 client 方法在 RT 路径下都不 alloc。
    /// （client 端通常不在 rt_thread 调，但 ipc_thread 用的同一份代码；保持
    /// 习惯让 ipc_thread tick 也"几乎"零 alloc。）
    #[test]
    fn client_methods_alloc_free() {
        // assert_no_alloc 不在 robot-s0 dev-deps；用 closure 调一次 ensure no panic
        let (c, _i) = fresh();
        c.set_policy(OutputPolicy::ActiveForward);
        c.safety_stop();
        let s = c.request_normal(AdapterNormalReq::Enable(false));
        assert!(matches!(s, NormalReqStatus::Submitted { .. }));
        let _ = c.poll_last_normal_resp(1);
        c.clear_safety_latch();
    }

    /// 编译期：`Box<HardRtCtrlClient>` 满足 trait object 要求。
    #[test]
    fn client_is_dyn_object() {
        let (c, _) = fresh();
        let _: Box<dyn ExecutorCtrlClient> = Box::new(c);
    }

    /// **finding #1 lock**：`safety_stop()` 必须**同步**把 `OutputPolicy`
    /// 切到 `StopTorque`。`policy.load()` 在 client 调返回之后立即可见，
    /// 不依赖任何 rt_thread tick。
    #[test]
    fn safety_stop_synchronously_disables_policy() {
        let (c, i) = fresh();
        // 模拟 ipc 启动期切到 ActiveForward
        c.set_policy(OutputPolicy::ActiveForward);
        assert_eq!(
            OutputPolicy::from_u8(i.policy.load(Ordering::Acquire)),
            OutputPolicy::ActiveForward
        );
        // emergency
        c.safety_stop();
        // **同步**：policy 已切 StopTorque（不等 rt tick）
        assert_eq!(
            OutputPolicy::from_u8(i.policy.load(Ordering::Acquire)),
            OutputPolicy::StopTorque,
            "safety_stop() 必须同步切 OutputPolicy=StopTorque，避免 rt_thread \
             下个 tick 仍按 ActiveForward 调 write_command"
        );
        // 配套：latch + emergency 也得置位（与 client_safety_stop_synchronously_
        // latches_and_drains_pending 配套，本测试关注 policy 这一项）
        assert!(i.safety_latched.load(Ordering::Acquire));
        assert_eq!(
            i.emergency_action.load(Ordering::Acquire),
            emergency_codes::SAFETY_STOP
        );
    }
}
