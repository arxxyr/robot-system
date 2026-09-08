//! 低频采样请求与串口边界回执；只控制测试 epoch，不写业务安全状态。

use std::sync::atomic::{AtomicU64, Ordering::SeqCst};

use robot_core::executor_ctrl::OutputSafetyState;

use crate::hard_rt_ctrl::{HardRtCtrlInner, NORMAL_IO_ACTIVE, SERIAL_ESTOP_PENDING};

use super::TracePage;

const MAX_SEQUENCE: u64 = u64::MAX >> 3;
const WINDOW_NS: u64 = 1_000_000_000;
const WRITING: u64 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u64)]
pub enum SampleMode {
    ArmInflight = 1,
    ArmIdle = 2,
    Disarm = 3,
}

impl SampleMode {
    fn decode(value: u64) -> Option<Self> {
        match value {
            1 => Some(Self::ArmInflight),
            2 => Some(Self::ArmIdle),
            3 => Some(Self::Disarm),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u64)]
pub enum SampleStatus {
    Empty = 0,
    Pending = 1,
    Applying = 2,
    Ready = 3,
    Disarmed = 4,
    Failed = 5,
}

impl SampleStatus {
    fn decode(word: u64) -> Self {
        match word & 7 {
            0 => Self::Empty,
            1 => Self::Pending,
            2 => Self::Applying,
            3 => Self::Ready,
            4 => Self::Disarmed,
            _ => Self::Failed,
        }
    }

    fn terminal(self) -> bool {
        matches!(self, Self::Ready | Self::Disarmed | Self::Failed)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u64)]
pub enum SampleFailure {
    InvalidRequest = 1,
    Busy = 2,
    StaleSequence = 3,
    AlreadyArmed = 4,
    PriorFailure = 5,
    BoundaryExpired = 6,
    IdleExpired = 7,
}

impl SampleFailure {
    fn decode(word: u64) -> Option<Self> {
        match word & 7 {
            1 => Some(Self::InvalidRequest),
            2 => Some(Self::Busy),
            3 => Some(Self::StaleSequence),
            4 => Some(Self::AlreadyArmed),
            5 => Some(Self::PriorFailure),
            6 => Some(Self::BoundaryExpired),
            7 => Some(Self::IdleExpired),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u64)]
pub enum SampleGateRelease {
    Safety = 2,
    Expired = 3,
    Disarmed = 4,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BoundarySnapshot {
    pub request_seq: u64,
    pub request_mode: Option<SampleMode>,
    pub request_deadline_ns: u64,
    pub receipt_seq: u64,
    pub mode: Option<SampleMode>,
    pub status: SampleStatus,
    pub ready_ns: u64,
    pub idle_deadline_ns: u64,
    pub armed_seq: u64,
    pub gate_active: bool,
    pub gate_release: Option<SampleGateRelease>,
    pub failure_seq: u64,
    pub failure: Option<SampleFailure>,
}

#[repr(C)]
pub(super) struct BoundaryWords {
    request: AtomicU64,
    request_deadline: AtomicU64,
    receipt: AtomicU64,
    receipt_mode: AtomicU64,
    ready_ns: AtomicU64,
    idle_deadline: AtomicU64,
    gate: AtomicU64,
    failure: AtomicU64,
}

const fn packed(sequence: u64, tag: u64) -> u64 {
    (sequence << 3) | tag
}

impl TracePage {
    /// 单 control 写者先占 Writing 再发布不可续期的请求；拒绝也留下 sticky 失败。
    pub fn request_boundary(
        &self,
        sequence: u64,
        mode: SampleMode,
        now: u64,
    ) -> Result<(), SampleFailure> {
        self.request_boundary_impl(sequence, mode, now, || {})
    }

    #[cfg(test)]
    pub(crate) fn request_boundary_after_writing(
        &self,
        sequence: u64,
        mode: SampleMode,
        now: u64,
        after_writing: impl FnOnce(),
    ) -> Result<(), SampleFailure> {
        self.request_boundary_impl(sequence, mode, now, after_writing)
    }

    fn request_boundary_impl(
        &self,
        sequence: u64,
        mode: SampleMode,
        now: u64,
        after_writing: impl FnOnce(),
    ) -> Result<(), SampleFailure> {
        let words = &self.words().boundary;
        let reject = |failure| {
            self.reject_boundary(sequence, failure);
            Err(failure)
        };
        let Some(deadline) = now.checked_add(WINDOW_NS) else {
            return reject(SampleFailure::InvalidRequest);
        };
        if sequence == 0 || sequence > MAX_SEQUENCE {
            return reject(SampleFailure::InvalidRequest);
        }
        let previous = words.request.load(SeqCst);
        let receipt = words.receipt.load(SeqCst);
        if previous & 7 == WRITING
            || (previous != 0
                && (previous >> 3 != receipt >> 3 || !SampleStatus::decode(receipt).terminal()))
        {
            return reject(SampleFailure::Busy);
        }
        if sequence <= previous >> 3 {
            return reject(SampleFailure::StaleSequence);
        }
        if mode != SampleMode::Disarm {
            if words.failure.load(SeqCst) != 0 {
                return reject(SampleFailure::PriorFailure);
            }
            if self.armed_seq() != 0 {
                return reject(SampleFailure::AlreadyArmed);
            }
        }
        if words
            .request
            .compare_exchange(previous, packed(sequence, WRITING), SeqCst, SeqCst)
            .is_err()
        {
            return reject(SampleFailure::Busy);
        }
        after_writing();
        words.request_deadline.store(deadline, SeqCst);
        words.request.store(packed(sequence, mode as u64), SeqCst);
        Ok(())
    }

    pub(crate) fn reject_boundary(&self, sequence: u64, failure: SampleFailure) {
        // 非法超界原序号由 control 日志保留；sticky 字只需让本进程后续 arm 永久拒绝。
        let _ = self.words().boundary.failure.compare_exchange(
            0,
            packed(sequence.clamp(1, MAX_SEQUENCE), failure as u64),
            SeqCst,
            SeqCst,
        );
    }

    /// 只由 serial 在真实事务边界调用；没有等待、分配或权威位写入。
    pub(crate) fn service_boundary(&self, ctrl: &HardRtCtrlInner, now: u64) -> bool {
        let words = &self.words().boundary;
        let gate = words.gate.load(SeqCst);
        if gate & 7 == 1 {
            let sequence = gate >> 3;
            if now >= words.idle_deadline.load(SeqCst) {
                self.reject_boundary(sequence, SampleFailure::IdleExpired);
                words
                    .gate
                    .store(packed(sequence, SampleGateRelease::Expired as u64), SeqCst);
                if words.receipt.load(SeqCst) == packed(sequence, SampleStatus::Ready as u64) {
                    words
                        .receipt
                        .store(packed(sequence, SampleStatus::Failed as u64), SeqCst);
                }
            } else if ctrl.safety_word.load(SeqCst) & SERIAL_ESTOP_PENDING != 0
                || ctrl.inhibit.load(SeqCst) != 0
                || ctrl.safety_state() == OutputSafetyState::StopCommanded
            {
                words
                    .gate
                    .store(packed(sequence, SampleGateRelease::Safety as u64), SeqCst);
            }
        }
        let request = words.request.load(SeqCst);
        let sequence = request >> 3;
        let Some(mode) = SampleMode::decode(request & 7) else {
            return self.sample_gate_active();
        };
        let receipt = words.receipt.load(SeqCst);
        if receipt >> 3 == sequence && SampleStatus::decode(receipt).terminal() {
            return self.sample_gate_active();
        }
        if now >= words.request_deadline.load(SeqCst) {
            self.complete_boundary(sequence, mode, now, Some(SampleFailure::BoundaryExpired));
            return self.sample_gate_active();
        }
        if ctrl.safety_word.load(SeqCst) & NORMAL_IO_ACTIVE != 0 {
            return self.sample_gate_active();
        }
        let failure = match mode {
            SampleMode::Disarm => None,
            SampleMode::ArmIdle if now.checked_add(WINDOW_NS).is_none() => {
                Some(SampleFailure::InvalidRequest)
            }
            _ if words.failure.load(SeqCst) != 0 => Some(SampleFailure::PriorFailure),
            _ if self.armed_seq() != 0 => Some(SampleFailure::AlreadyArmed),
            _ => None,
        };
        self.complete_boundary(sequence, mode, now, failure);
        self.sample_gate_active()
    }

    fn complete_boundary(
        &self,
        sequence: u64,
        mode: SampleMode,
        now: u64,
        failure: Option<SampleFailure>,
    ) {
        let words = &self.words().boundary;
        words
            .receipt
            .store(packed(sequence, SampleStatus::Applying as u64), SeqCst);
        words.receipt_mode.store(mode as u64, SeqCst);
        words.ready_ns.store(now, SeqCst);
        let status = match failure {
            Some(failure) => {
                self.reject_boundary(sequence, failure);
                SampleStatus::Failed
            }
            None => match mode {
                SampleMode::Disarm => {
                    let gate = words.gate.load(SeqCst);
                    if gate & 7 == 1 {
                        words.gate.store(
                            packed(gate >> 3, SampleGateRelease::Disarmed as u64),
                            SeqCst,
                        );
                    }
                    self.arm(0);
                    SampleStatus::Disarmed
                }
                SampleMode::ArmIdle | SampleMode::ArmInflight => {
                    let idle = mode == SampleMode::ArmIdle;
                    words
                        .idle_deadline
                        .store(if idle { now + WINDOW_NS } else { 0 }, SeqCst);
                    words
                        .gate
                        .store(if idle { packed(sequence, 1) } else { 0 }, SeqCst);
                    self.arm(sequence);
                    SampleStatus::Ready
                }
            },
        };
        words.receipt.store(packed(sequence, status as u64), SeqCst);
    }

    pub(super) fn sample_gate_active(&self) -> bool {
        self.words().boundary.gate.load(SeqCst) & 7 == 1
    }

    /// 只接受同一请求与终态回执的完整快照；调用者下一次轮询可以重试 None。
    #[must_use]
    pub fn boundary_snapshot(&self) -> Option<BoundarySnapshot> {
        let words = &self.words().boundary;
        let request = words.request.load(SeqCst);
        let receipt = words.receipt.load(SeqCst);
        if request & 7 == WRITING || SampleStatus::decode(receipt) == SampleStatus::Applying {
            return None;
        }
        let gate = words.gate.load(SeqCst);
        let failure = words.failure.load(SeqCst);
        let snapshot = BoundarySnapshot {
            request_seq: request >> 3,
            request_mode: SampleMode::decode(request & 7),
            request_deadline_ns: words.request_deadline.load(SeqCst),
            receipt_seq: receipt >> 3,
            mode: SampleMode::decode(words.receipt_mode.load(SeqCst)),
            status: if request >> 3 == receipt >> 3 {
                SampleStatus::decode(receipt)
            } else {
                SampleStatus::Pending
            },
            ready_ns: words.ready_ns.load(SeqCst),
            idle_deadline_ns: words.idle_deadline.load(SeqCst),
            armed_seq: self.armed_seq(),
            gate_active: gate & 7 == 1,
            gate_release: match gate & 7 {
                2 => Some(SampleGateRelease::Safety),
                3 => Some(SampleGateRelease::Expired),
                4 => Some(SampleGateRelease::Disarmed),
                _ => None,
            },
            failure_seq: failure >> 3,
            failure: SampleFailure::decode(failure),
        };
        (request == words.request.load(SeqCst)
            && receipt == words.receipt.load(SeqCst)
            && gate == words.gate.load(SeqCst)
            && failure == words.failure.load(SeqCst))
        .then_some(snapshot)
    }
}
