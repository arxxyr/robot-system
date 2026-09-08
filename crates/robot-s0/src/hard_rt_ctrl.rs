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
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};

use crate::state_machine::FaultCode;
use robot_core::executor_ctrl::{
    AdapterErr, AdapterNormalReq, AdapterResp, ExecutorCtrlClient, NormalReqStatus, OutputPolicy,
    OutputSafetyState, emergency_codes, encode_normal_req, normal_codes, pack_completed,
    pack_pending_normal, unpack_completed, unpack_pending_normal,
};
use robot_core::executor_ctrl::{EStopOutcome, SerialEmergencyCtrl};
use robot_core::serial_services::{NormalAbortTarget, SEQ_MASK};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CtrlKind {
    HardRt,
    SerialRt,
}

pub(crate) const SERIAL_ESTOP_PENDING: u32 = 1 << 8;
pub(crate) const IPC_ESTOP_NOTICE: u32 = 1 << 9;
pub(crate) const NORMAL_IO_ACTIVE: u32 = 1 << 10;
const SAFETY_STATE_MASK: u32 = 0xff;
pub(crate) const PENDING_CANCELLED: u64 = 1 << 63;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SerialPending {
    pub action: ActionRecord,
    pub cancelled: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct InvalidSerialPending(pub u64);

pub(crate) const fn decode_serial_pending(
    raw: u64,
) -> Result<Option<SerialPending>, InvalidSerialPending> {
    if raw == 0 {
        return Ok(None);
    }
    let payload = raw & !PENDING_CANCELLED;
    let (action, arg, seq) = unpack_pending_normal(payload);
    if payload >> 56 != 0 || seq == 0 || seq > SEQ_MASK || action == normal_codes::NONE {
        return Err(InvalidSerialPending(raw));
    }
    Ok(Some(SerialPending {
        action: ActionRecord {
            valid: true,
            action,
            arg,
            seq,
        },
        cancelled: raw & PENDING_CANCELLED != 0,
    }))
}

/// 活跃记录采用低位 valid、action、arg、seq；高七位永远为零。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ActionRecord {
    pub valid: bool,
    pub action: u8,
    pub arg: u8,
    pub seq: u64,
}

impl ActionRecord {
    pub(crate) const fn pack(self) -> u64 {
        (self.valid as u64)
            | ((self.action as u64) << 1)
            | ((self.arg as u64) << 9)
            | ((self.seq & SEQ_MASK) << 17)
    }

    pub(crate) const fn unpack(raw: u64) -> Self {
        Self {
            valid: raw & 1 != 0,
            action: ((raw >> 1) & 0xff) as u8,
            arg: ((raw >> 9) & 0xff) as u8,
            seq: (raw >> 17) & SEQ_MASK,
        }
    }
}

/// 完成记录的低两位依次为 valid、success，随后为 action、arg、40 位 seq。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CompletedEffect {
    pub action: ActionRecord,
    pub success: bool,
}

impl CompletedEffect {
    pub(crate) const fn pack(self) -> u64 {
        (self.action.valid as u64)
            | ((self.success as u64) << 1)
            | ((self.action.action as u64) << 2)
            | ((self.action.arg as u64) << 10)
            | ((self.action.seq & SEQ_MASK) << 18)
    }

    pub(crate) const fn unpack(raw: u64) -> Self {
        Self {
            action: ActionRecord {
                valid: raw & 1 != 0,
                action: ((raw >> 2) & 0xff) as u8,
                arg: ((raw >> 10) & 0xff) as u8,
                seq: (raw >> 18) & SEQ_MASK,
            },
            success: raw & 2 != 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum FaultOrigin {
    Periodic,
    NormalAction,
    PendingTimeout,
    InvariantViolation,
    Confirm,
    StopNotDelivered,
    BootUntrusted,
    ConnectFailed,
    PreflightFailed,
    PreflightTimeout,
    EStopEscalation,
}

impl FaultOrigin {
    const fn from_u8(raw: u8) -> Self {
        match raw {
            0 => Self::Periodic,
            1 => Self::NormalAction,
            2 => Self::PendingTimeout,
            4 => Self::Confirm,
            5 => Self::StopNotDelivered,
            6 => Self::BootUntrusted,
            7 => Self::ConnectFailed,
            8 => Self::PreflightFailed,
            9 => Self::PreflightTimeout,
            10 => Self::EStopEscalation,
            _ => Self::InvariantViolation,
        }
    }
}

/// 单词绑定代次与故障内容；位 0..31 为 generation，其后为 code/detail/origin。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MailboxSnapshot {
    pub generation: u32,
    pub code: FaultCode,
    pub detail: u8,
    pub origin: FaultOrigin,
}

impl MailboxSnapshot {
    pub(crate) const fn pack(self) -> u64 {
        let code = match self.code {
            FaultCode::AdapterErr => 1,
            FaultCode::CmdDeadlineExpired => 2,
            FaultCode::LimitViolation => 3,
            FaultCode::WatchdogTimeout => 4,
            FaultCode::RtOverrun => 5,
            FaultCode::SafetyStopFailed => 6,
            FaultCode::CommFault => 7,
        };
        (self.generation as u64)
            | (code << 32)
            | ((self.detail as u64) << 40)
            | ((self.origin as u64) << 48)
    }

    pub(crate) const fn unpack(raw: u64) -> Option<Self> {
        let generation = (raw & 0xffff_ffff) as u32;
        if generation == 0 {
            return None;
        }
        let code = match (raw >> 32) & 0xff {
            1 => FaultCode::AdapterErr,
            2 => FaultCode::CmdDeadlineExpired,
            3 => FaultCode::LimitViolation,
            4 => FaultCode::WatchdogTimeout,
            5 => FaultCode::RtOverrun,
            6 => FaultCode::SafetyStopFailed,
            _ => FaultCode::CommFault,
        };
        Some(Self {
            generation,
            code,
            detail: ((raw >> 40) & 0xff) as u8,
            origin: FaultOrigin::from_u8(((raw >> 48) & 0xff) as u8),
        })
    }
}

/// 只收紧可输出策略。CAS 失败必须按新值重判，绝不覆盖并发 Disabled。
pub(crate) fn tighten_estop_policy(policy: &AtomicU8) {
    tighten_estop_policy_impl(policy, || {});
}

fn tighten_estop_policy_impl(policy: &AtomicU8, mut before_cas: impl FnMut()) {
    let mut old = policy.load(Ordering::Acquire);
    loop {
        match OutputPolicy::from_u8(old) {
            OutputPolicy::ReadyHold
            | OutputPolicy::ActiveForward
            | OutputPolicy::HoldCurrent
            | OutputPolicy::ZeroCommand => {}
            OutputPolicy::Disabled | OutputPolicy::StopTorque => return,
        }
        before_cas();
        match policy.compare_exchange(
            old,
            OutputPolicy::StopTorque.to_u8(),
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return,
            Err(actual) => old = actual,
        }
    }
}

#[cfg(test)]
pub(crate) fn tighten_estop_policy_before_cas(policy: &AtomicU8, hook: impl FnMut()) {
    tighten_estop_policy_impl(policy, hook);
}

/// 串口普通 IO 的独立抑制原因；任何一位均拒绝普通操作。
pub(crate) mod inhibit {
    pub(crate) const COMM_FAULT: u8 = 1;
    pub(crate) const ADAPTER_FAULT: u8 = 1 << 1;
    pub(crate) const STOP_FAILED: u8 = 1 << 2;
    pub(crate) const CONVERGING: u8 = 1 << 3;
    pub(crate) const FAULT_PENDING: u8 = 1 << 4;
    pub(crate) const BOOT: u8 = 1 << 5;
}

// ============================================================================
// HardRtCtrlInner
// ============================================================================

/// HardRT 路径的共享状态。client 端 (`HardRtCtrlClient`) 只读 `Arc<Inner>`；
/// rt_thread 持 `Arc<Inner>` 并在主循环内 swap / store。
#[derive(Debug)]
pub struct HardRtCtrlInner {
    pub(crate) ctrl_kind: CtrlKind,
    pub(crate) safety_word: AtomicU32,
    pub(crate) active_action: AtomicU64,
    pub(crate) completed_effect: AtomicU64,
    pub(crate) abort_request: AtomicU64,
    pub(crate) fault_mailbox: AtomicU64,
    pub(crate) fault_ack: AtomicU32,
    pub(crate) inhibit: AtomicU8,
    pub(crate) session_clean: AtomicBool,
    pub(crate) rx_trust: AtomicBool,
    pub(crate) comm_level: AtomicU8,
    pub(crate) l0_since_ns: AtomicU64,
    pub(crate) liveness_counter: AtomicU64,
    pub(crate) estop_requests: AtomicU32,
    pub(crate) boot_outcome: AtomicU8,
    /// serial 首次物理出口：先 Relaxed 写时间，再 Release 置 reached；读者须先 Acquire。
    pub(crate) boot_first_exit_ns: AtomicU64,
    pub(crate) boot_exit_reached: AtomicBool,
    pub(crate) boot_deadline_expired: AtomicBool,
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
    /// `seq_counter`。HardRT 保留 fetch_add；SerialRT strong CAS 在 40 位边界
    /// 拒绝后不再推进。正常取号即便 pending CAS 失败也废弃序号，允许空洞。
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

/// 主线程启动等待只读视图，不暴露底层控制原子。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SerialBootProgress {
    /// 0=Pending，1=Passed，2=Faulted。
    pub outcome: u8,
    pub first_exit: Option<robot_core::time::MonoRawNs>,
    pub fault_acknowledged: bool,
}

// 生产只使用 old；测试构建额外保存同一次 CAS 的真实失败数与返回时刻。
struct SafetyCasObservation {
    old: u32,
    #[cfg(feature = "test-comm-fault-hook")]
    retries: u64,
    #[cfg(feature = "test-comm-fault-hook")]
    stamp_ns: u64,
}

impl HardRtCtrlInner {
    #[must_use]
    pub fn serial_boot_progress(&self) -> SerialBootProgress {
        let outcome = self.boot_outcome.load(Ordering::Acquire);
        let first_exit = self
            .boot_exit_reached
            .load(Ordering::Acquire)
            .then(|| robot_core::time::MonoRawNs(self.boot_first_exit_ns.load(Ordering::Relaxed)));
        let fault_acknowledged = self
            .mailbox_snapshot()
            .is_none_or(|mail| self.fault_ack.load(Ordering::Acquire) == mail.generation);
        SerialBootProgress {
            outcome,
            first_exit,
            fault_acknowledged,
        }
    }

    /// 主线程超期通知，serial 仍独占最终收敛及至少一次 Safety 尝试。
    pub fn expire_serial_boot(&self) {
        self.boot_deadline_expired.store(true, Ordering::Release);
    }

    #[must_use]
    pub fn serial_liveness(&self) -> u64 {
        self.liveness_counter.load(Ordering::Relaxed)
    }
    /// 出厂默认：policy=Disabled / safety_state=TorqueUnknown / emergency=NONE /
    /// pending=0 / seq_counter=1（避免初值与 valid seq 0 冲突；poll 看到
    /// `last_completed_seq < 1` 表示"从未完成过"）/ last_completed=0 /
    /// safety_latched=false。
    #[must_use]
    pub fn new() -> Self {
        Self::with_kind(CtrlKind::HardRt)
    }

    /// SerialRT 新协议构造入口；生产选择在完整启动接线后切换。
    #[must_use]
    pub fn new_serial() -> Self {
        Self::with_kind(CtrlKind::SerialRt)
    }

    fn with_kind(ctrl_kind: CtrlKind) -> Self {
        Self {
            ctrl_kind,
            safety_word: AtomicU32::new(0),
            active_action: AtomicU64::new(0),
            completed_effect: AtomicU64::new(0),
            abort_request: AtomicU64::new(0),
            fault_mailbox: AtomicU64::new(0),
            fault_ack: AtomicU32::new(0),
            inhibit: AtomicU8::new(match ctrl_kind {
                CtrlKind::HardRt => 0,
                CtrlKind::SerialRt => inhibit::BOOT,
            }),
            session_clean: AtomicBool::new(true),
            rx_trust: AtomicBool::new(false),
            comm_level: AtomicU8::new(0),
            l0_since_ns: AtomicU64::new(0),
            liveness_counter: AtomicU64::new(0),
            estop_requests: AtomicU32::new(0),
            boot_outcome: AtomicU8::new(0),
            boot_first_exit_ns: AtomicU64::new(0),
            boot_exit_reached: AtomicBool::new(false),
            boot_deadline_expired: AtomicBool::new(false),
            policy: AtomicU8::new(OutputPolicy::Disabled.to_u8()),
            safety_state: AtomicU8::new(OutputSafetyState::TorqueUnknown.to_u8()),
            emergency_action: AtomicU8::new(emergency_codes::NONE),
            pending_normal: AtomicU64::new(0),
            seq_counter: AtomicU64::new(1),
            last_completed: AtomicU64::new(0),
            safety_latched: AtomicBool::new(false),
        }
    }

    /// 所有观测方共用同一权威来源选择，避免 pub 误读 legacy 字节。
    #[must_use]
    pub fn safety_state(&self) -> OutputSafetyState {
        let raw = match self.ctrl_kind {
            CtrlKind::HardRt => self.safety_state.load(Ordering::Acquire),
            CtrlKind::SerialRt => {
                (self.safety_word.load(Ordering::Acquire) & SAFETY_STATE_MASK) as u8
            }
        };
        OutputSafetyState::from_u8(raw)
    }

    fn take_next_seq(&self) -> Option<u64> {
        match self.ctrl_kind {
            CtrlKind::HardRt => Some(self.seq_counter.fetch_add(1, Ordering::AcqRel)),
            CtrlKind::SerialRt => {
                let mut seq = self.seq_counter.load(Ordering::Acquire);
                loop {
                    // 越界后不再递增，连 u64 本身也不允许绕回可用序号。
                    if seq == 0 || seq > SEQ_MASK {
                        return None;
                    }
                    match self.seq_counter.compare_exchange(
                        seq,
                        seq + 1,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        Ok(_) => return Some(seq),
                        Err(actual) => seq = actual,
                    }
                }
            }
        }
    }

    fn update_safety_word(&self, update: impl Fn(u32) -> u32) -> u32 {
        self.update_safety_word_with_hook(update, || {})
    }

    fn update_safety_word_with_hook(
        &self,
        update: impl Fn(u32) -> u32,
        before_cas: impl FnMut(),
    ) -> u32 {
        self.update_safety_word_observed(update, before_cas).old
    }

    fn update_safety_word_observed(
        &self,
        update: impl Fn(u32) -> u32,
        mut before_cas: impl FnMut(),
    ) -> SafetyCasObservation {
        #[cfg(feature = "test-comm-fault-hook")]
        let mut retries = 0_u64;
        let mut old = self.safety_word.load(Ordering::Acquire);
        loop {
            let new = update(old);
            before_cas();
            match self
                .safety_word
                .compare_exchange(old, new, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(previous) => {
                    return SafetyCasObservation {
                        old: previous,
                        #[cfg(feature = "test-comm-fault-hook")]
                        retries,
                        #[cfg(feature = "test-comm-fault-hook")]
                        stamp_ns: crate::comm_fault_trace::now(),
                    };
                }
                Err(actual) => {
                    #[cfg(feature = "test-comm-fault-hook")]
                    {
                        retries = retries.saturating_add(1);
                    }
                    old = actual;
                }
            }
        }
    }

    pub(crate) fn try_acquire_normal_io(&self) -> bool {
        #[cfg(feature = "test-comm-fault-hook")]
        let mut retries = 0_u64;
        let mut old = self.safety_word.load(Ordering::Acquire);
        loop {
            if old & (SERIAL_ESTOP_PENDING | NORMAL_IO_ACTIVE) != 0 {
                #[cfg(feature = "test-comm-fault-hook")]
                self.trace_now(
                    crate::comm_fault_trace::TraceKind::NormalAcquire,
                    retries,
                    0,
                );
                return false;
            }
            match self.safety_word.compare_exchange(
                old,
                old | NORMAL_IO_ACTIVE,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    #[cfg(feature = "test-comm-fault-hook")]
                    self.trace_now(
                        crate::comm_fault_trace::TraceKind::NormalAcquire,
                        retries,
                        1,
                    );
                    return true;
                }
                Err(actual) => {
                    #[cfg(feature = "test-comm-fault-hook")]
                    {
                        retries = retries.saturating_add(1);
                    }
                    old = actual;
                }
            }
        }
    }

    pub(crate) fn release_normal_io(&self) {
        let observed = self.update_safety_word_observed(|old| old & !NORMAL_IO_ACTIVE, || {});
        #[cfg(feature = "test-comm-fault-hook")]
        self.trace_cas(
            crate::comm_fault_trace::TraceKind::NormalRelease,
            &observed,
            u64::from(observed.old & NORMAL_IO_ACTIVE != 0),
        );
        #[cfg(not(feature = "test-comm-fault-hook"))]
        let _ = observed;
    }

    /// 正常动作的完成不得覆盖在它执行期间已发布的 EStop 安全态。
    pub(crate) fn set_serial_normal_safety(&self, state: OutputSafetyState) {
        self.update_safety_word(|old| match old & SERIAL_ESTOP_PENDING {
            0 => (old & !SAFETY_STATE_MASK) | u32::from(state.to_u8()),
            _ => old,
        });
    }

    fn cancel_pending(&self) {
        self.cancel_pending_impl(|| {});
    }

    fn cancel_pending_impl(&self, mut before_cas: impl FnMut()) {
        let mut old = self.pending_normal.load(Ordering::Acquire);
        loop {
            if old == 0 || old & PENDING_CANCELLED != 0 {
                return;
            }
            before_cas();
            match self.pending_normal.compare_exchange(
                old,
                old | PENDING_CANCELLED,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(actual) => old = actual,
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn cancel_pending_before_cas(&self, hook: impl FnMut()) {
        self.cancel_pending_impl(hook);
    }

    pub(crate) fn set_serial_safety(&self, state: OutputSafetyState) {
        self.update_safety_word(|old| (old & !SAFETY_STATE_MASK) | u32::from(state.to_u8()));
    }

    pub(crate) fn claim_serial_estop(&self) -> bool {
        let observed = self.update_safety_word_observed(|old| old & !SERIAL_ESTOP_PENDING, || {});
        let claimed = observed.old & SERIAL_ESTOP_PENDING != 0;
        #[cfg(feature = "test-comm-fault-hook")]
        self.trace_cas(
            crate::comm_fault_trace::TraceKind::SerialClaim,
            &observed,
            u64::from(claimed),
        );
        claimed
    }

    pub(crate) fn claim_ipc_estop(&self) -> bool {
        let observed = self.update_safety_word_observed(|old| old & !IPC_ESTOP_NOTICE, || {});
        let claimed = observed.old & IPC_ESTOP_NOTICE != 0;
        #[cfg(feature = "test-comm-fault-hook")]
        self.trace_cas(
            crate::comm_fault_trace::TraceKind::IpcClaim,
            &observed,
            u64::from(claimed),
        );
        claimed
    }

    /// 仅清掉自己处理的请求；并发新请求必须保留到下一轮。
    pub(crate) fn clear_abort(&self, observed: u64) -> bool {
        self.abort_request
            .compare_exchange(observed, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(crate) fn mailbox_snapshot(&self) -> Option<MailboxSnapshot> {
        MailboxSnapshot::unpack(self.fault_mailbox.load(Ordering::Acquire))
    }

    /// 唯一调用方为 IPC，传入它实际提交过的快照代次。
    pub(crate) fn ack_fault(&self, generation: u32) {
        self.fault_ack.store(generation, Ordering::Release);
    }

    /// serial 单写者入口。饱和后仍收紧状态与抑制位，返回 None 供调用方记诊断。
    pub(crate) fn publish_fault(
        &self,
        state: OutputSafetyState,
        reasons: u8,
        code: FaultCode,
        detail: u8,
        origin: FaultOrigin,
    ) -> Option<u32> {
        self.set_serial_safety(state);
        self.inhibit
            .fetch_or(reasons | inhibit::FAULT_PENDING, Ordering::Release);
        let generation = self.mailbox_snapshot().map_or(0, |mail| mail.generation);
        if generation >= u32::MAX - 1 {
            return None;
        }
        let generation = generation + 1;
        #[cfg(feature = "test-comm-fault-hook")]
        let before_publish = crate::comm_fault_trace::now();
        self.fault_mailbox.store(
            MailboxSnapshot {
                generation,
                code,
                detail,
                origin,
            }
            .pack(),
            Ordering::Release,
        );
        #[cfg(feature = "test-comm-fault-hook")]
        {
            use crate::comm_fault_trace::{self as trace, TraceKind};
            let after_publish = trace::now();
            let mut context = self.trace_context();
            context.generation = generation;
            trace::emit(
                TraceKind::MailboxPublishing,
                before_publish,
                context,
                crate::comm_fault_trace::fault_code(code),
                u64::from(detail),
            );
            trace::emit(
                TraceKind::MailboxPublished,
                after_publish,
                context,
                crate::comm_fault_trace::fault_code(code),
                u64::from(detail),
            );
            trace::bind_comm_to_generation(generation);
        }
        Some(generation)
    }

    #[cfg(feature = "test-comm-fault-hook")]
    pub(crate) fn trace_context(&self) -> crate::comm_fault_trace::TraceContext {
        let active = ActionRecord::unpack(self.active_action.load(Ordering::Acquire));
        crate::comm_fault_trace::TraceContext {
            source: 0,
            normal_seq: if active.valid { active.seq } else { 0 },
            generation: self.mailbox_snapshot().map_or(0, |mail| mail.generation),
        }
    }

    #[cfg(feature = "test-comm-fault-hook")]
    pub(crate) fn trace_now(&self, kind: crate::comm_fault_trace::TraceKind, a: u64, b: u64) {
        let stamp = crate::comm_fault_trace::now();
        crate::comm_fault_trace::emit(kind, stamp, self.trace_context(), a, b);
    }

    #[cfg(feature = "test-comm-fault-hook")]
    fn trace_cas(
        &self,
        kind: crate::comm_fault_trace::TraceKind,
        observed: &SafetyCasObservation,
        b: u64,
    ) {
        crate::comm_fault_trace::emit(
            kind,
            observed.stamp_ns,
            self.trace_context(),
            observed.retries,
            b,
        );
    }

    /// 只有当前代次的 ACK 才能解开 mailbox 抑制；其它故障原因保持。
    pub(crate) fn clear_acked_fault_pending(&self) -> bool {
        let Some(mail) = self.mailbox_snapshot() else {
            return false;
        };
        if self.fault_ack.load(Ordering::Acquire) != mail.generation {
            return false;
        }
        self.inhibit
            .fetch_and(!inhibit::FAULT_PENDING, Ordering::Release);
        true
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
    fn abort_pending_normal(&self, target: NormalAbortTarget) {
        if self.inner.ctrl_kind == CtrlKind::SerialRt {
            // 非法序号保持为非法词，由 serial 的不变量路径收敛，不能掩码为合法序号。
            self.inner
                .abort_request
                .store(target.encode().unwrap_or(u64::MAX), Ordering::Release);
        }
    }
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
        let Some(seq) = self.inner.take_next_seq() else {
            return NormalReqStatus::Unsupported("SerialRT 请求序号超出 40 位有效范围");
        };
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
        self.inner.safety_state()
    }
}

impl SerialEmergencyCtrl for HardRtCtrlClient {
    fn estop_monotonic(&self) -> EStopOutcome {
        self.estop_monotonic_impl(|| {})
    }
}

impl HardRtCtrlClient {
    #[cfg(test)]
    pub(crate) fn estop_monotonic_before_cas(&self, hook: impl FnMut()) -> EStopOutcome {
        self.estop_monotonic_impl(hook)
    }

    fn estop_monotonic_impl(&self, hook: impl FnMut()) -> EStopOutcome {
        self.inner.safety_latched.store(true, Ordering::Release);
        self.inner.cancel_pending();
        self.inner.estop_requests.fetch_add(1, Ordering::Relaxed);
        let observed = self.inner.update_safety_word_observed(
            |old| {
                let state = OutputSafetyState::from_u8((old & SAFETY_STATE_MASK) as u8);
                let next = match state {
                    OutputSafetyState::TorqueUnknown
                    | OutputSafetyState::TorqueOn
                    | OutputSafetyState::TorqueOffConfirmed => OutputSafetyState::StopCommanded,
                    _ => state,
                };
                (old & !SAFETY_STATE_MASK)
                    | u32::from(next.to_u8())
                    | SERIAL_ESTOP_PENDING
                    | IPC_ESTOP_NOTICE
            },
            hook,
        );
        let old = observed.old;
        // 成功 CAS 之后只计算返回值，不能再写 policy 或任何权威原子。
        let outcome = match (
            old & SERIAL_ESTOP_PENDING != 0,
            OutputSafetyState::from_u8((old & SAFETY_STATE_MASK) as u8),
        ) {
            (true, _) | (false, OutputSafetyState::StopCommanded) => EStopOutcome::Merged,
            (false, OutputSafetyState::StopFailed | OutputSafetyState::AdapterUncertain) => {
                EStopOutcome::LatchOnly
            }
            _ => EStopOutcome::StopIssued,
        };
        #[cfg(feature = "test-comm-fault-hook")]
        self.inner.trace_cas(
            crate::comm_fault_trace::TraceKind::PrimitiveCommitted,
            &observed,
            u64::from(old & NORMAL_IO_ACTIVE != 0)
                | match outcome {
                    EStopOutcome::StopIssued => 0,
                    EStopOutcome::Merged => 2,
                    EStopOutcome::LatchOnly => 4,
                },
        );
        outcome
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

    #[test]
    fn cancelled_serial_pending_preserves_identity_and_rejects_every_reserved_bit() {
        for seq in [1, 17, SEQ_MASK] {
            let raw = pack_pending_normal(normal_codes::SET_MODE, 4, seq);
            for cancelled in [false, true] {
                let tagged = raw | if cancelled { PENDING_CANCELLED } else { 0 };
                let decoded = decode_serial_pending(tagged).unwrap().unwrap();
                assert_eq!(decoded.cancelled, cancelled);
                assert_eq!(
                    decoded.action,
                    ActionRecord {
                        valid: true,
                        action: normal_codes::SET_MODE,
                        arg: 4,
                        seq
                    }
                );
                for bit in 56..63 {
                    assert!(decode_serial_pending(tagged | (1 << bit)).is_err());
                }
            }
        }
        assert_eq!(decode_serial_pending(0), Ok(None));
        for raw in [
            PENDING_CANCELLED,
            PENDING_CANCELLED | 3,
            pack_pending_normal(0, 1, 7),
            pack_pending_normal(3, 0, 0),
        ] {
            assert!(decode_serial_pending(raw).is_err());
        }
    }

    #[test]
    fn cancel_cas_reloads_after_consumption_and_never_restores_the_old_payload() {
        let ctrl = HardRtCtrlInner::new_serial();
        let first = pack_pending_normal(normal_codes::ENABLE, 0, 8);
        let next = pack_pending_normal(normal_codes::SET_MODE, 2, 9);
        ctrl.pending_normal.store(first, Ordering::Release);
        let mut interleaved = false;
        ctrl.cancel_pending_before_cas(|| {
            if !interleaved {
                interleaved = true;
                assert_eq!(ctrl.pending_normal.swap(0, Ordering::AcqRel), first);
                ctrl.pending_normal.store(next, Ordering::Release);
            }
        });
        assert_eq!(
            ctrl.pending_normal.load(Ordering::Acquire),
            next | PENDING_CANCELLED
        );
        ctrl.pending_normal.store(first, Ordering::Release);
        ctrl.cancel_pending_before_cas(|| {
            ctrl.pending_normal.store(0, Ordering::Release);
        });
        assert_eq!(ctrl.pending_normal.load(Ordering::Acquire), 0);
    }

    #[test]
    fn normal_safety_commit_cannot_overwrite_a_published_estop_state_or_other_bits() {
        let ctrl = HardRtCtrlInner::new_serial();
        ctrl.set_serial_normal_safety(OutputSafetyState::TorqueOn);
        assert_eq!(ctrl.safety_state(), OutputSafetyState::TorqueOn);
        let published = u32::from(OutputSafetyState::StopCommanded.to_u8())
            | SERIAL_ESTOP_PENDING
            | IPC_ESTOP_NOTICE
            | (1 << 20);
        ctrl.safety_word.store(published, Ordering::Release);
        ctrl.set_serial_normal_safety(OutputSafetyState::TorqueOffConfirmed);
        assert_eq!(ctrl.safety_word.load(Ordering::Acquire), published);
    }

    #[test]
    fn action_records_use_the_exact_40_bit_layout() {
        for seq in [1, 2, SEQ_MASK] {
            for valid in [false, true] {
                let action = ActionRecord {
                    valid,
                    action: 5,
                    arg: 255,
                    seq,
                };
                assert_eq!(
                    action.pack(),
                    u64::from(valid) | (5 << 1) | (255 << 9) | (seq << 17)
                );
                assert_eq!(ActionRecord::unpack(action.pack()), action);
                assert_eq!(action.pack() >> 57, 0);
                for success in [false, true] {
                    let completed = CompletedEffect { action, success };
                    assert_eq!(
                        completed.pack(),
                        u64::from(valid)
                            | (u64::from(success) << 1)
                            | (5 << 2)
                            | (255 << 10)
                            | (seq << 18)
                    );
                    assert_eq!(CompletedEffect::unpack(completed.pack()), completed);
                    assert_eq!(completed.pack() >> 58, 0);
                }
            }
        }
    }

    #[test]
    fn exhausted_serial_sequence_never_increments_or_wraps() {
        for invalid in [0, SEQ_MASK + 1, u64::MAX] {
            let inner = Arc::new(HardRtCtrlInner::new_serial());
            let client = HardRtCtrlClient::new(Arc::clone(&inner));
            inner.seq_counter.store(invalid, Ordering::Release);
            for _ in 0..8 {
                assert!(matches!(
                    client.request_normal(AdapterNormalReq::Enable(false)),
                    NormalReqStatus::Unsupported(_)
                ));
                assert_eq!(inner.seq_counter.load(Ordering::Acquire), invalid);
                assert_eq!(inner.pending_normal.load(Ordering::Acquire), 0);
            }
        }
    }

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
