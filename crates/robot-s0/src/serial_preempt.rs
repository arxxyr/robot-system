//! `SerialRT` 事务边界抢占、分片等待与有截止时间的进度观测。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use robot_core::serial_services::{
    AbortReason, BusOpClass, CommErrorKind, NormalAbortTarget, PreemptProbe,
};
use robot_core::time::{DurationNs, MonoRawNs, RobotClock};

use crate::hard_rt_ctrl::{HardRtCtrlInner, SERIAL_ESTOP_PENDING};

/// 服务在 spawn 前构造，serial 是上下文唯一写者，adapter 在同线程读取。
pub struct SerialPreemptProbe {
    clock: Arc<dyn RobotClock>,
    ctrl: Arc<HardRtCtrlInner>,
    class: AtomicU8,
    seq: AtomicU64,
    lease_deadline_ns: AtomicU64,
    confirmation_trusted: AtomicBool,
    observed_comm_error: AtomicU8,
}

impl SerialPreemptProbe {
    #[must_use]
    pub fn new(clock: Arc<dyn RobotClock>, ctrl: Arc<HardRtCtrlInner>) -> Self {
        Self {
            clock,
            ctrl,
            class: AtomicU8::new(BusOpClass::Connect as u8),
            seq: AtomicU64::new(0),
            lease_deadline_ns: AtomicU64::new(0),
            confirmation_trusted: AtomicBool::new(false),
            observed_comm_error: AtomicU8::new(0),
        }
    }

    /// C3 phase 接线后为每次 adapter 调用安装上下文；不得在调用途中换域。
    #[allow(dead_code)] // C3 phase 接线后撤除。
    pub(crate) fn set_context(
        &self,
        class: BusOpClass,
        seq: u64,
        deadline: MonoRawNs,
        confirmation_trusted: bool,
    ) {
        self.class.store(class as u8, Ordering::Relaxed);
        self.seq.store(seq, Ordering::Relaxed);
        self.lease_deadline_ns.store(deadline.0, Ordering::Relaxed);
        self.confirmation_trusted
            .store(confirmation_trusted, Ordering::Relaxed);
    }

    /// 每次 Confirm 完成后先消费事实，再判断证据可信；不依赖诊断是否丢失。
    #[allow(dead_code)] // C3 Confirm 接线后撤除。
    pub(crate) fn take_comm_error(&self) -> Option<CommErrorKind> {
        match self.observed_comm_error.swap(0, Ordering::AcqRel) {
            0 => None,
            encoded => Some(CommErrorKind::from_u8(encoded - 1)),
        }
    }
}

impl PreemptProbe for SerialPreemptProbe {
    fn should_abort(&self, class: BusOpClass) -> Option<AbortReason> {
        // 非正常域是收敛自身，不受 emergency 或普通请求 deadline 中断。
        if !class.is_normal() {
            return None;
        }
        if self.ctrl.safety_word.load(Ordering::Acquire) & SERIAL_ESTOP_PENDING != 0 {
            return Some(AbortReason::Emergency);
        }
        if class != BusOpClass::NormalEffectful {
            return None;
        }
        let abort = NormalAbortTarget::decode(self.ctrl.abort_request.load(Ordering::Acquire));
        match abort {
            Ok(Some(NormalAbortTarget::Submitted { seq }))
                if seq == self.seq.load(Ordering::Relaxed) =>
            {
                Some(AbortReason::Timeout)
            }
            // 非法词不能继续普通动作，由 serial 返回后按不变量违例收敛。
            Err(_) => Some(AbortReason::Timeout),
            _ => None,
        }
    }

    fn begin_transaction(&self, class: BusOpClass) -> Result<(), AbortReason> {
        if let Some(reason) = self.should_abort(class) {
            return Err(reason);
        }
        if class.is_normal() && !self.ctrl.try_acquire_normal_io() {
            return Err(AbortReason::Emergency);
        }
        Ok(())
    }

    fn finish_transaction(&self, class: BusOpClass) {
        if class.is_normal() {
            self.ctrl.release_normal_io();
        }
    }

    fn checkpoint(&self) {
        if self.clock.now().0 < self.lease_deadline_ns.load(Ordering::Relaxed) {
            self.ctrl.liveness_counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn sleep_or_abort(&self, class: BusOpClass, dur: DurationNs) -> Result<(), AbortReason> {
        let deadline = self.clock.now().0.saturating_add(dur.0);
        loop {
            if let Some(reason) = self.should_abort(class) {
                return Err(reason);
            }
            let remaining = deadline.saturating_sub(self.clock.now().0);
            if remaining == 0 {
                return Ok(());
            }
            self.clock.sleep_for(DurationNs(remaining.min(5_000_000)));
        }
    }

    fn operation_class(&self) -> BusOpClass {
        match self.class.load(Ordering::Relaxed) {
            0 => BusOpClass::NormalEffectful,
            1 => BusOpClass::PeriodicRead,
            2 => BusOpClass::PeriodicWrite,
            3 => BusOpClass::Safety,
            4 => BusOpClass::Confirm,
            _ => BusOpClass::Connect,
        }
    }

    fn confirmation_trusted(&self) -> bool {
        self.confirmation_trusted.load(Ordering::Relaxed)
    }

    fn report_comm_error(&self, kind: CommErrorKind) {
        // 记录首个事实；后续成功或其它错误不覆盖本次已发现的 hygiene 失败。
        let _ = self.observed_comm_error.compare_exchange(
            0,
            kind.to_u8() + 1,
            Ordering::Release,
            Ordering::Relaxed,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hard_rt_ctrl::{HardRtCtrlClient, NORMAL_IO_ACTIVE};
    use robot_core::executor_ctrl::{ExecutorCtrlClient, SerialEmergencyCtrl};
    use robot_core::time::MockClock;

    #[test]
    fn transaction_release_preserves_emergency_and_checkpoint_keeps_lease() {
        let clock = Arc::new(MockClock::default());
        let ctrl = Arc::new(HardRtCtrlInner::new_serial());
        let client = HardRtCtrlClient::new(Arc::clone(&ctrl));
        let probe = SerialPreemptProbe::new(clock.clone(), ctrl.clone());
        probe.set_context(BusOpClass::NormalEffectful, 9, MonoRawNs(100), false);
        assert_eq!(probe.begin_transaction(BusOpClass::NormalEffectful), Ok(()));
        probe.checkpoint();
        assert_ne!(
            ctrl.safety_word.load(Ordering::Acquire) & NORMAL_IO_ACTIVE,
            0
        );
        client.estop_monotonic();
        probe.finish_transaction(BusOpClass::NormalEffectful);
        assert_eq!(
            ctrl.safety_word.load(Ordering::Acquire) & NORMAL_IO_ACTIVE,
            0
        );
        assert_eq!(
            probe.begin_transaction(BusOpClass::PeriodicRead),
            Err(AbortReason::Emergency)
        );
        assert_eq!(probe.begin_transaction(BusOpClass::Safety), Ok(()));
        probe.finish_transaction(BusOpClass::Safety);
        assert_eq!(ctrl.liveness_counter.load(Ordering::Relaxed), 1);
        clock.set(MonoRawNs(100));
        probe.checkpoint();
        assert_eq!(ctrl.liveness_counter.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn matching_timeout_and_sliced_sleep_use_the_correct_operation_domain() {
        let clock = Arc::new(MockClock::default());
        let ctrl = Arc::new(HardRtCtrlInner::new_serial());
        let client = HardRtCtrlClient::new(ctrl.clone());
        let probe = SerialPreemptProbe::new(clock.clone(), ctrl.clone());
        probe.set_context(BusOpClass::NormalEffectful, 9, MonoRawNs(100_000_000), true);
        client.abort_pending_normal(NormalAbortTarget::Submitted { seq: 8 });
        assert_eq!(
            probe.sleep_or_abort(BusOpClass::NormalEffectful, DurationNs(12_000_000)),
            Ok(())
        );
        assert_eq!(clock.now(), MonoRawNs(12_000_000));
        assert_eq!(
            ctrl.safety_word.load(Ordering::Acquire) & NORMAL_IO_ACTIVE,
            0
        );
        client.abort_pending_normal(NormalAbortTarget::Submitted { seq: 9 });
        assert_eq!(
            probe.should_abort(BusOpClass::NormalEffectful),
            Some(AbortReason::Timeout)
        );
        for class in [
            BusOpClass::PeriodicRead,
            BusOpClass::PeriodicWrite,
            BusOpClass::Safety,
            BusOpClass::Confirm,
            BusOpClass::Connect,
        ] {
            assert_eq!(probe.should_abort(class), None);
        }
        client.estop_monotonic();
        assert_eq!(
            probe.should_abort(BusOpClass::NormalEffectful),
            Some(AbortReason::Emergency)
        );
        assert!(probe.confirmation_trusted());
        assert_eq!(probe.operation_class(), BusOpClass::NormalEffectful);
    }

    #[test]
    fn hygiene_fact_survives_later_success_until_explicit_consumption() {
        let probe = SerialPreemptProbe::new(
            Arc::new(MockClock::default()),
            Arc::new(HardRtCtrlInner::new_serial()),
        );
        assert_eq!(probe.take_comm_error(), None);
        probe.report_comm_error(CommErrorKind::DiscardFailed);
        probe.report_comm_error(CommErrorKind::FrameCorrupt);
        assert_eq!(probe.take_comm_error(), Some(CommErrorKind::DiscardFailed));
        assert_eq!(probe.take_comm_error(), None);
    }
}
