//! 同一 A2 权限实例的执行器控制与原序号通道；不保存第二安全权限字。

#![allow(clippy::doc_markdown)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};

use crate::state_machine::FaultCode;
use robot_core::control_authority::{
    ControlAuthority, ExecutionViewSlot, InhibitReason, SafetyTightening, StartViewSlot,
};
use robot_core::executor_ctrl::{
    AdapterErr, AdapterNormalReq, AdapterResp, ExecutorCtrlClient, NormalReqStatus, OutputPolicy,
    OutputSafetyState, emergency_codes, encode_normal_req, normal_codes, pack_pending_normal,
    unpack_completed, unpack_pending_normal,
};
use robot_core::executor_ctrl::{EStopOutcome, SerialEmergencyCtrl};
use robot_core::serial_services::{NormalAbortTarget, SEQ_MASK};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CtrlKind {
    HardRt,
    SerialRt,
}

#[cfg(any(test, feature = "test-comm-fault-hook"))]
pub(crate) const SERIAL_ESTOP_PENDING: u32 = 1 << 8;
#[cfg(any(test, feature = "test-comm-fault-hook"))]
pub(crate) const IPC_ESTOP_NOTICE: u32 = 1 << 9;
#[cfg(any(test, feature = "test-comm-fault-hook"))]
pub(crate) const NORMAL_IO_ACTIVE: u32 = 1 << 10;
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
    #[cfg(test)]
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
    authority: Arc<ControlAuthority>,
    start_slot: StartViewSlot,
    execution_slot: ExecutionViewSlot,
    pub(crate) active_action: AtomicU64,
    pub(crate) completed_effect: AtomicU64,
    pub(crate) abort_request: AtomicU64,
    pub(crate) fault_mailbox: AtomicU64,
    pub(crate) fault_ack: AtomicU32,
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
}

/// 主线程启动等待只读视图，不暴露底层控制原子。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SerialBootProgress {
    /// 0=Pending，1=Passed，2=Faulted。
    pub outcome: u8,
    pub first_exit: Option<robot_core::time::MonoRawNs>,
    pub fault_acknowledged: bool,
}

impl HardRtCtrlInner {
    /// 仅供全部 worker 已 join 的主线程判定干净退出；不授予 IO 或解除 EStop。
    /// 故障邮箱在本轮进程内只发布、不清零，因此零值同时排除已 ACK 的历史故障。
    #[must_use]
    pub fn clean_rx_restart_ready(&self) -> bool {
        let state = self.authority.snapshot();
        self.ctrl_kind == CtrlKind::SerialRt
            && self.boot_outcome.load(Ordering::Acquire) == 1
            && !self.boot_deadline_expired.load(Ordering::Acquire)
            && self.rx_trust.load(Ordering::Acquire)
            && self.session_clean.load(Ordering::Acquire)
            && self.fault_mailbox.load(Ordering::Acquire) == 0
            && self.pending_normal.load(Ordering::Acquire) == 0
            && self.active_action.load(Ordering::Acquire) == 0
            && self.abort_request.load(Ordering::Acquire) == 0
            && state.shutdown
            && state.disabled
            && !state.exhausted
            && state.phase == Some(robot_core::control_authority::Phase::Idle)
            && state.inhibits.is_empty()
            && !state.normal_io_active
            && !state.serial_estop_pending
            && state.safety == OutputSafetyState::TorqueOffConfirmed
    }

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
    pub fn new(authority: Arc<ControlAuthority>) -> Self {
        Self::with_kind(CtrlKind::HardRt, authority)
    }

    /// SerialRT 新协议构造入口；生产选择在完整启动接线后切换。
    #[must_use]
    pub fn new_serial(authority: Arc<ControlAuthority>) -> Self {
        Self::with_kind(CtrlKind::SerialRt, authority)
    }

    fn with_kind(ctrl_kind: CtrlKind, authority: Arc<ControlAuthority>) -> Self {
        Self {
            ctrl_kind,
            start_slot: StartViewSlot::new(&authority),
            execution_slot: ExecutionViewSlot::new(&authority),
            authority,
            active_action: AtomicU64::new(0),
            completed_effect: AtomicU64::new(0),
            abort_request: AtomicU64::new(0),
            fault_mailbox: AtomicU64::new(0),
            fault_ack: AtomicU32::new(0),
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
            emergency_action: AtomicU8::new(emergency_codes::NONE),
            pending_normal: AtomicU64::new(0),
            seq_counter: AtomicU64::new(1),
            last_completed: AtomicU64::new(0),
        }
    }

    /// 初始化期共享同一个对象；tick 仅借用，不 clone Arc。
    #[must_use]
    pub fn authority(&self) -> &Arc<ControlAuthority> {
        &self.authority
    }
    #[must_use]
    pub fn start_slot(&self) -> &StartViewSlot {
        &self.start_slot
    }
    #[must_use]
    pub fn execution_slot(&self) -> &ExecutionViewSlot {
        &self.execution_slot
    }
    #[must_use]
    pub fn safety_state(&self) -> OutputSafetyState {
        self.authority.snapshot().safety
    }
    #[must_use]
    pub fn safety_latched(&self) -> bool {
        self.authority.snapshot().latch
    }
    #[must_use]
    pub fn inhibit_bits(&self) -> u8 {
        self.authority.snapshot().inhibits.bits()
    }

    /// 仅投影给既有诊断；不提供写入或凭此投影取 IO 权限的入口。
    #[must_use]
    #[cfg(any(test, feature = "test-comm-fault-hook"))]
    pub(crate) fn safety_word(&self) -> u32 {
        let s = self.authority.snapshot();
        u32::from(s.safety.to_u8())
            | if s.serial_estop_pending {
                SERIAL_ESTOP_PENDING
            } else {
                0
            }
            | if s.ipc_estop_notice {
                IPC_ESTOP_NOTICE
            } else {
                0
            }
            | if s.normal_io_active {
                NORMAL_IO_ACTIVE
            } else {
                0
            }
    }

    pub(crate) fn tighten_inhibits(&self, reasons: u8) {
        for reason in InhibitReason::ALL {
            if reasons & reason.bit() != 0 {
                let _ = self.authority.tighten_inhibit(reason);
            }
        }
    }

    /// 仅接受安全收紧，可信完成必须带真实操作开始的 SafetyObservation。
    pub(crate) fn tighten_safety(&self, state: OutputSafetyState) {
        let tightening = match state {
            OutputSafetyState::StopCommanded => SafetyTightening::StopCommanded,
            OutputSafetyState::StopFailed => SafetyTightening::StopFailed,
            OutputSafetyState::AdapterUncertain => SafetyTightening::AdapterUncertain,
            _ => return,
        };
        let _ = self.authority.tighten_safety(tightening);
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

    fn cancel_pending(&self) {
        self.cancel_pending_impl(|| {});
    }

    fn cancel_pending_impl(&self, mut before_cas: impl FnMut()) {
        // HardRT 的原序号占满高 48 位，不能借用最高位作取消标记。
        // 原请求由执行器在安全停止后真实拒绝，或由原 seq 的 CAS 撤回。
        if self.ctrl_kind == CtrlKind::HardRt {
            return;
        }
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

    pub(crate) fn claim_serial_estop(&self) -> bool {
        #[cfg(feature = "test-comm-fault-hook")]
        {
            let observed = self.authority.claim_serial_estop_observed();
            self.trace_now(
                crate::comm_fault_trace::TraceKind::SerialClaim,
                observed.failed_cas.unwrap_or(u64::MAX),
                u64::from(observed.outcome),
            );
            observed.outcome
        }
        #[cfg(not(feature = "test-comm-fault-hook"))]
        self.authority.claim_serial_estop()
    }

    pub(crate) fn claim_ipc_estop(&self) -> bool {
        #[cfg(feature = "test-comm-fault-hook")]
        {
            let observed = self.authority.claim_ipc_estop_observed();
            self.trace_now(
                crate::comm_fault_trace::TraceKind::IpcClaim,
                observed.failed_cas.unwrap_or(u64::MAX),
                u64::from(observed.outcome),
            );
            observed.outcome
        }
        #[cfg(not(feature = "test-comm-fault-hook"))]
        self.authority.claim_ipc_estop()
    }

    /// CAS 仅取消尚未被执行器取走的原 Enable(true)；None 不等执行完成。
    pub(crate) fn decode_pending(
        &self,
        raw: u64,
    ) -> Result<Option<SerialPending>, InvalidSerialPending> {
        match self.ctrl_kind {
            CtrlKind::SerialRt => decode_serial_pending(raw),
            CtrlKind::HardRt => {
                if raw == 0 {
                    return Ok(None);
                }
                let (action, arg, seq) = unpack_pending_normal(raw);
                if seq == 0 || action == normal_codes::NONE {
                    return Err(InvalidSerialPending(raw));
                }
                Ok(Some(SerialPending {
                    action: ActionRecord {
                        valid: true,
                        action,
                        arg,
                        seq,
                    },
                    cancelled: false,
                }))
            }
        }
    }

    /// 原通道的序号宽度不变；取消只比较并取回该笔原请求。
    pub(crate) fn try_cancel_unclaimed(&self, seq: u64) -> CancelPendingResult {
        let mut raw = self.pending_normal.load(Ordering::Acquire);
        loop {
            let pending = match self.decode_pending(raw) {
                Ok(Some(pending)) => pending,
                Ok(None) => return CancelPendingResult::NotTaken,
                Err(e) => return CancelPendingResult::Invalid(e),
            };
            if pending.action.seq != seq
                || pending.action.action != normal_codes::ENABLE
                || pending.action.arg != 1
            {
                return CancelPendingResult::NotTaken;
            }
            match self
                .pending_normal
                .compare_exchange(raw, 0, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return CancelPendingResult::Taken(pending.action),
                Err(actual) => raw = actual,
            }
        }
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
    #[cfg(test)]
    pub(crate) fn publish_fault(
        &self,
        state: OutputSafetyState,
        reasons: u8,
        code: FaultCode,
        detail: u8,
        origin: FaultOrigin,
    ) -> Option<u32> {
        self.publish_fault_with_epoch(state, reasons, code, detail, origin)
            .0
    }

    pub(crate) fn publish_fault_with_epoch(
        &self,
        state: OutputSafetyState,
        reasons: u8,
        code: FaultCode,
        detail: u8,
        origin: FaultOrigin,
    ) -> (Option<u32>, robot_core::control_authority::PermissionEpoch) {
        self.tighten_safety(state);
        self.tighten_inhibits(reasons);
        let epoch = self
            .authority
            .tighten_inhibit(InhibitReason::FaultPending)
            .epoch;
        let generation = self.mailbox_snapshot().map_or(0, |mail| mail.generation);
        if generation >= u32::MAX - 1 {
            return (None, epoch);
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
        (Some(generation), epoch)
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

    /// 只有当前代次的 ACK 才能解开 mailbox 抑制；其它故障原因保持。
    pub(crate) fn clear_acked_fault_pending_at(
        &self,
        epoch: robot_core::control_authority::PermissionEpoch,
    ) -> Option<robot_core::control_authority::PermissionEpoch> {
        let permit = match self
            .authority
            .capture_inhibit_release(InhibitReason::FaultPending)
        {
            Ok(permit) if permit.epoch() == epoch => permit,
            _ => return None,
        };
        let mail = self.mailbox_snapshot()?;
        if self.fault_ack.load(Ordering::Acquire) != mail.generation {
            return None;
        }
        self.authority
            .try_release_inhibit(permit)
            .ok()
            .map(|released| released.epoch)
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
    #[must_use]
    pub fn authority(&self) -> &Arc<ControlAuthority> {
        self.inner.authority()
    }
    #[must_use]
    pub fn start_slot(&self) -> &StartViewSlot {
        self.inner.start_slot()
    }
    #[must_use]
    pub fn execution_slot(&self) -> &ExecutionViewSlot {
        self.inner.execution_slot()
    }
    pub(crate) fn try_cancel_unclaimed(&self, seq: u64) -> CancelPendingResult {
        self.inner.try_cancel_unclaimed(seq)
    }
    /// 权限已由真实事件收紧后，只请求实际执行器停止，不再次推进 epoch。
    pub fn request_stop_execution(&self) {
        self.inner.cancel_pending();
        self.inner
            .emergency_action
            .store(emergency_codes::SAFETY_STOP, Ordering::Release);
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
        let _ = self.estop_impl(|| {});
        self.inner
            .policy
            .store(OutputPolicy::StopTorque.to_u8(), Ordering::Release);
        self.request_stop_execution();
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
        if action == normal_codes::ENABLE && arg == 1 && self.inner.safety_latched() {
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

    fn safety_state(&self) -> OutputSafetyState {
        self.inner.safety_state()
    }
}

impl HardRtCtrlClient {
    fn estop_impl(&self, after_cancel: impl FnOnce()) -> EStopOutcome {
        self.inner.cancel_pending();
        after_cancel();
        self.inner.estop_requests.fetch_add(1, Ordering::Relaxed);
        #[cfg(feature = "test-comm-fault-hook")]
        let observed = self.inner.authority.estop_observed();
        #[cfg(feature = "test-comm-fault-hook")]
        let committed = observed.outcome;
        #[cfg(not(feature = "test-comm-fault-hook"))]
        let committed = self.inner.authority.estop();
        #[cfg(feature = "test-comm-fault-hook")]
        self.inner.trace_now(
            crate::comm_fault_trace::TraceKind::PrimitiveCommitted,
            observed.failed_cas.unwrap_or(u64::MAX),
            u64::from(committed.normal_io_active)
                | match committed.outcome {
                    EStopOutcome::StopIssued => 0,
                    EStopOutcome::Merged => 2,
                    EStopOutcome::LatchOnly => 4,
                },
        );
        committed.outcome
    }
}

impl SerialEmergencyCtrl for HardRtCtrlClient {
    fn estop_monotonic(&self) -> EStopOutcome {
        self.estop_impl(|| {})
    }
}

#[cfg(test)]
impl HardRtCtrlClient {
    /// 只在取消原 packed 请求与真实 A2 调用之间建立交错；不是 A2 内部 CAS 注入。
    pub(crate) fn estop_monotonic_after_cancel(&self, hook: impl FnOnce()) -> EStopOutcome {
        self.estop_impl(hook)
    }
}

#[derive(Debug)]
pub(crate) enum CancelPendingResult {
    Taken(ActionRecord),
    NotTaken,
    Invalid(InvalidSerialPending),
}

/// 执行器栈内的唯一收尾责任；真实 finish 拒绝时整个原件留至下一 tick。
#[derive(Debug)]
pub(crate) struct EnableFinishPending {
    pub execution: robot_core::control_authority::EnableExecution,
    pub action: ActionRecord,
    pub result: robot_core::control_authority::EnableResult,
    pub response: AdapterResp,
    /// 只由实际 adapter 调用路径设置；不声称任何一笔 wire 已成功。
    pub adapter_called: bool,
    recovery: RecoveryArranged,
}

/// 执行器正常退出时携回的原收尾责任；不证明设备已停止或 IPC 已退休。
/// 不可复制、外造或取走 pending，唯一生产归宿是实际 join 的退出聚合。
#[must_use = "执行器退出结果必须保留到实际退出聚合，不能丢弃原执行责任"]
#[derive(Debug)]
pub struct ExecutorExit {
    pending_enable: Option<EnableFinishPending>,
}

impl ExecutorExit {
    /// 只报告是否仍持有原 Enable；false 本身不是设备安全或关闭完成凭据。
    #[must_use]
    pub fn has_pending_enable(&self) -> bool {
        self.pending_enable.is_some()
    }

    /// 只用于实际执行循环早退/末尾与 SerialTick 的按值退出出口。
    pub(crate) fn from_pending(pending_enable: Option<EnableFinishPending>) -> Self {
        Self { pending_enable }
    }
}

#[derive(Debug)]
enum RecoveryArranged {
    NotArranged,
    AfterThisCall,
}

impl EnableFinishPending {
    pub(crate) fn new(
        execution: robot_core::control_authority::EnableExecution,
        action: ActionRecord,
        result: robot_core::control_authority::EnableResult,
        response: AdapterResp,
        adapter_called: bool,
    ) -> Self {
        Self {
            execution,
            action,
            result,
            response,
            adapter_called,
            recovery: RecoveryArranged::NotArranged,
        }
    }

    #[allow(
        clippy::needless_pass_by_value,
        reason = "同次恢复事实必须按值消费，不能借用后再次关联另一完成件"
    )]
    pub(crate) fn record_serial_recovery(
        &mut self,
        arranged: crate::serial_convergence::EnableRecoveryArranged,
    ) {
        if self.adapter_called && arranged.covers(self.action.seq) {
            self.recovery = RecoveryArranged::AfterThisCall;
        }
    }

    /// 仅 HardRT 本线程实际 safety_stop_rt 返回的分支调用，不能依据旧安全快照。
    pub(crate) fn record_hard_rt_stop_returned(&mut self) {
        if self.adapter_called {
            self.recovery = RecoveryArranged::AfterThisCall;
        }
    }
}

/// 已消费真实 Execution、尚未发布原 seq 的唯一完成件。
/// 字段私有且不可复制；先安装停止责任，再一次性发布原响应。
#[must_use]
#[derive(Debug)]
pub(crate) struct EnableFinished {
    action: ActionRecord,
    response: AdapterResp,
    finish: robot_core::control_authority::EnableFinish,
    adapter_called: bool,
    recovery: RecoveryArranged,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EnableCompensation {
    NotRequired,
    Required,
}

impl EnableFinished {
    /// 回调只能安装当前执行器的停止责任；它不是 Off 或退休证明。
    /// 原 seq 的 Release 发布位于所有收紧和责任安装之后。
    pub(crate) fn publish_after_compensation(
        self,
        ctrl: &HardRtCtrlInner,
        before_publish: impl FnOnce(AdapterResp, EnableCompensation),
    ) {
        let compensation = if self.adapter_called
            && !self.finish.torque_on_published
            && matches!(self.recovery, RecoveryArranged::NotArranged)
        {
            ctrl.tighten_safety(OutputSafetyState::StopCommanded);
            EnableCompensation::Required
        } else {
            EnableCompensation::NotRequired
        };
        before_publish(self.response, compensation);
        crate::serial_convergence::publish_completion(ctrl, self.action, self.response);
    }
}

pub(crate) fn start_enable_execution(
    ctrl: &HardRtCtrlInner,
) -> Result<robot_core::control_authority::EnableExecution, AdapterErr> {
    let view = ctrl.start_slot.take().ok_or(AdapterErr::AuthorityRevoked)?;
    ctrl.authority
        .start_enable(view)
        .map_err(|reason| match reason {
            robot_core::control_authority::Refusal::SerialEStopPending => {
                AdapterErr::PreemptedByEmergency
            }
            _ => AdapterErr::AuthorityRevoked,
        })
}

pub(crate) fn finish_enable_owned(
    active: EnableFinishPending,
    ctrl: &HardRtCtrlInner,
) -> Result<EnableFinished, EnableFinishPending> {
    let EnableFinishPending {
        execution,
        action,
        result,
        response,
        adapter_called,
        recovery,
    } = active;
    match ctrl.authority.finish_enable(execution, result) {
        Ok(finish) => Ok(EnableFinished {
            action,
            response,
            finish,
            adapter_called,
            recovery,
        }),
        Err(refused) => Err(EnableFinishPending {
            execution: refused.token,
            action,
            result,
            response,
            adapter_called,
            recovery,
        }),
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
pub(crate) fn test_authority(serial: bool) -> Arc<ControlAuthority> {
    use robot_core::control_authority::{AuthorityInit, InhibitSet};
    Arc::new(
        ControlAuthority::new(AuthorityInit::new(
            false,
            OutputSafetyState::TorqueOffConfirmed,
            if serial {
                InhibitSet::EMPTY.with(InhibitReason::Boot)
            } else {
                InhibitSet::EMPTY
            },
        ))
        .unwrap(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use robot_core::adapter::ControlMode;
    use robot_core::executor_ctrl::{AdapterErr, pack_completed};

    fn clean_serial_exit() -> HardRtCtrlInner {
        use robot_core::control_authority::{AuthorityInit, InhibitSet};
        let authority = Arc::new(
            ControlAuthority::new(AuthorityInit::new(
                true,
                OutputSafetyState::TorqueOffConfirmed,
                InhibitSet::EMPTY,
            ))
            .unwrap(),
        );
        let _ = authority.shutdown();
        let ctrl = HardRtCtrlInner::new_serial(authority);
        ctrl.boot_outcome.store(1, Ordering::Release);
        ctrl.rx_trust.store(true, Ordering::Release);
        ctrl
    }

    #[test]
    fn clean_rx_restart_requires_trust_and_no_fault_history() {
        let ctrl = clean_serial_exit();
        assert!(ctrl.clean_rx_restart_ready());
        assert!(ctrl.safety_latched(), "续授权判据不要求解除 EStop");
        ctrl.boot_outcome.store(0, Ordering::Release);
        assert!(!ctrl.clean_rx_restart_ready());
        ctrl.boot_outcome.store(2, Ordering::Release);
        assert!(!ctrl.clean_rx_restart_ready());
        ctrl.boot_outcome.store(1, Ordering::Release);
        ctrl.boot_deadline_expired.store(true, Ordering::Release);
        assert!(!ctrl.clean_rx_restart_ready());
        ctrl.boot_deadline_expired.store(false, Ordering::Release);
        ctrl.rx_trust.store(false, Ordering::Release);
        assert!(!ctrl.clean_rx_restart_ready());
        ctrl.rx_trust.store(true, Ordering::Release);
        ctrl.session_clean.store(false, Ordering::Release);
        assert!(!ctrl.clean_rx_restart_ready());
        ctrl.session_clean.store(true, Ordering::Release);
        let fault = MailboxSnapshot {
            generation: 1,
            code: FaultCode::CommFault,
            detail: 0,
            origin: FaultOrigin::Periodic,
        };
        ctrl.fault_mailbox.store(fault.pack(), Ordering::Release);
        ctrl.fault_ack.store(1, Ordering::Release);
        assert!(!ctrl.clean_rx_restart_ready(), "已 ACK 故障仍不得续授权");
    }

    #[test]
    fn clean_rx_restart_rejects_pending_or_uncertain_execution() {
        let ctrl = clean_serial_exit();
        for word in [
            &ctrl.pending_normal,
            &ctrl.active_action,
            &ctrl.abort_request,
        ] {
            word.store(1, Ordering::Release);
            assert!(!ctrl.clean_rx_restart_ready());
            word.store(0, Ordering::Release);
        }
        assert!(ctrl.clean_rx_restart_ready());
        let _ = ctrl.authority().tighten_inhibit(InhibitReason::CommFault);
        assert!(!ctrl.clean_rx_restart_ready());
        let ctrl = clean_serial_exit();
        let _ = ctrl
            .authority()
            .tighten_safety(SafetyTightening::AdapterUncertain);
        assert!(!ctrl.clean_rx_restart_ready());
    }

    #[test]
    fn clean_rx_restart_is_unavailable_before_shutdown_or_with_io_held() {
        use robot_core::control_authority::{AuthorityInit, InhibitSet, NormalIoIntent};
        let authority = Arc::new(
            ControlAuthority::new(AuthorityInit::new(
                true,
                OutputSafetyState::TorqueOffConfirmed,
                InhibitSet::EMPTY,
            ))
            .unwrap(),
        );
        let ctrl = HardRtCtrlInner::new_serial(authority);
        ctrl.boot_outcome.store(1, Ordering::Release);
        ctrl.rx_trust.store(true, Ordering::Release);
        assert!(!ctrl.clean_rx_restart_ready());
        let read = ctrl
            .authority()
            .acquire_normal_io(NormalIoIntent::PeriodicRead)
            .unwrap();
        let _ = ctrl.authority().shutdown();
        assert!(!ctrl.clean_rx_restart_ready());
        ctrl.authority().release_normal_io(read).unwrap();
        assert!(ctrl.clean_rx_restart_ready());
    }

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
        let ctrl = HardRtCtrlInner::new_serial(crate::hard_rt_ctrl::test_authority(true));
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
        use robot_core::control_authority::SafetyOperation;
        let ctrl = HardRtCtrlInner::new_serial(test_authority(false));
        let observation = ctrl
            .authority()
            .begin_safety_observation(SafetyOperation::Disable);
        let _ = ctrl.authority().estop();
        let before = ctrl.authority().snapshot();
        assert!(
            ctrl.authority()
                .complete_safety_observation(observation)
                .is_err()
        );
        assert_eq!(ctrl.authority().snapshot(), before);
        assert!(before.latch && before.serial_estop_pending && before.ipc_estop_notice);
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
            let inner = Arc::new(HardRtCtrlInner::new_serial(
                crate::hard_rt_ctrl::test_authority(true),
            ));
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
        let inner = Arc::new(HardRtCtrlInner::new(crate::hard_rt_ctrl::test_authority(
            false,
        )));
        let client = HardRtCtrlClient::new(Arc::clone(&inner));
        (client, inner)
    }

    #[test]
    fn default_state_is_safe() {
        let inner = HardRtCtrlInner::new(Arc::new(
            ControlAuthority::new(robot_core::control_authority::AuthorityInit::new(
                false,
                OutputSafetyState::TorqueUnknown,
                robot_core::control_authority::InhibitSet::EMPTY,
            ))
            .unwrap(),
        ));
        assert_eq!(
            OutputPolicy::from_u8(inner.policy.load(Ordering::Acquire)),
            OutputPolicy::Disabled
        );
        assert_eq!(inner.safety_state(), OutputSafetyState::TorqueUnknown);
        assert_eq!(
            inner.emergency_action.load(Ordering::Acquire),
            emergency_codes::NONE
        );
        assert_eq!(inner.pending_normal.load(Ordering::Acquire), 0);
        assert_eq!(inner.seq_counter.load(Ordering::Acquire), 1);
        assert_eq!(inner.last_completed.load(Ordering::Acquire), 0);
        assert!(!inner.safety_latched());
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
        let _ = i.authority().estop();

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

    /// 控制 client 没有清 latch 入口，原 A2 owner 完成真实提交才改变保护位。
    #[test]
    fn only_owned_clear_commit_unblocks_enable_submission() {
        let (client, inner) = fresh();
        let authority = inner.authority();
        let _ = authority.estop();
        assert!(matches!(
            client.request_normal(AdapterNormalReq::Enable(true)),
            NormalReqStatus::Unsupported(_)
        ));
        assert!(authority.claim_serial_estop());
        let observation = authority
            .begin_safety_observation(robot_core::control_authority::SafetyOperation::ConfirmOff);
        authority.complete_safety_observation(observation).unwrap();
        let owner = authority.reserve_clear().unwrap();
        let finalizing = authority.commit_clear(owner).unwrap();
        assert!(!inner.safety_latched());
        let cleanup = authority.authorize_finalize(finalizing).unwrap();
        authority.retire_clear_cleanup(cleanup).unwrap();
        assert!(matches!(
            client.request_normal(AdapterNormalReq::Enable(true)),
            NormalReqStatus::Submitted { .. }
        ));
    }

    /// 这里只核 client 调用的有限行为，真实 allocator 反证由独立 guard 用例执行。
    #[test]
    fn client_methods_preserve_finite_control_operations() {
        let (client, _) = fresh();
        client.set_policy(OutputPolicy::ActiveForward);
        client.safety_stop();
        assert!(matches!(
            client.request_normal(AdapterNormalReq::Enable(false)),
            NormalReqStatus::Submitted { .. }
        ));
        assert_eq!(client.poll_last_normal_resp(1), None);
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
        assert!(i.safety_latched());
        assert_eq!(
            i.emergency_action.load(Ordering::Acquire),
            emergency_codes::SAFETY_STOP
        );
    }

    #[test]
    fn actual_finish_refusal_keeps_execution_and_original_reply_until_lease_release() {
        use robot_core::control_authority::{EnableResult, NormalIoIntent, Refusal};
        let (client, ctrl) = fresh();
        let (owner, view) = ctrl.authority().reserve_enable().unwrap();
        ctrl.start_slot().post(view).unwrap();
        let execution = start_enable_execution(&ctrl).unwrap();
        let lease = ctrl
            .authority()
            .acquire_normal_io(NormalIoIntent::EnableStep(execution.view()))
            .unwrap();
        let active = EnableFinishPending::new(
            execution,
            ActionRecord {
                valid: true,
                action: normal_codes::ENABLE,
                arg: 1,
                seq: 97,
            },
            EnableResult::Succeeded,
            AdapterResp::Ok,
            false,
        );
        let active = finish_enable_owned(active, &ctrl).unwrap_err();
        assert_eq!(active.action.seq, 97);
        let mut exit = ExecutorExit::from_pending(Some(active));
        assert!(exit.has_pending_enable());
        assert_eq!(exit.pending_enable.as_ref().unwrap().action.seq, 97);
        assert_eq!(client.poll_last_normal_resp(97), None);
        let rejected = ctrl.authority().retire_enable(owner).unwrap_err();
        assert!(matches!(rejected.reason, Refusal::WrongPhase(_)));
        ctrl.authority().release_normal_io(lease).unwrap();
        // 仅模块内夹具拆原件继续验证同一 finish；生产没有任何公开恢复/取走接口。
        let active = exit.pending_enable.take().unwrap();
        assert!(!exit.has_pending_enable());
        let finished = finish_enable_owned(active, &ctrl).unwrap();
        assert_eq!(client.poll_last_normal_resp(97), None);
        finished.publish_after_compensation(&ctrl, |_, required| {
            assert_eq!(required, EnableCompensation::NotRequired);
        });
        assert_eq!(client.poll_last_normal_resp(97), Some(AdapterResp::Ok));
        ctrl.authority().commit_enable(rejected.token).unwrap();
    }
}
