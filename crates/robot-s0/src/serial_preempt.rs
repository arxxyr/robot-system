//! `SerialRT` 事务边界抢占、分片等待与有截止时间的进度观测。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use robot_core::serial_services::{
    AbortReason, BusLease, BusOpClass, CommErrorKind, LeasedPreemptProbe, NormalAbortTarget,
    PreemptProbe,
};
use robot_core::time::{DurationNs, MonoRawNs, RobotClock};

use crate::hard_rt_ctrl::HardRtCtrlInner;
use robot_core::control_authority::{NonEnergizingControl, NormalIoIntent, Refusal};

/// 每次真实调用安装意图；EnableStep 的视图仅从同 ctrl 的执行槽读取。
#[derive(Clone, Copy, Debug)]
#[repr(u8)]
pub(crate) enum InstalledIntent {
    None = 0,
    PeriodicRead = 1,
    Motion = 2,
    EnableStep = 3,
    Connect = 4,
    Disconnect = 5,
    Disable = 6,
}

/// 服务在 spawn 前构造，serial 是上下文唯一写者，adapter 在同线程读取。
pub struct SerialPreemptProbe {
    clock: Arc<dyn RobotClock>,
    ctrl: Arc<HardRtCtrlInner>,
    class: AtomicU8,
    intent: AtomicU8,
    seq: AtomicU64,
    lease_deadline_ns: AtomicU64,
    confirmation_trusted: AtomicBool,
    observed_comm_error: AtomicU8,
    tick_has_call: AtomicBool,
    tick_overdue: AtomicBool,
}

impl SerialPreemptProbe {
    #[cfg(feature = "test-comm-fault-hook")]
    fn finish_injected_delay(
        &self,
        lease: BusLease,
        delayed: Result<(), crate::comm_fault_hook::DelayFailure>,
    ) -> Result<BusLease, AbortReason> {
        if delayed.is_err() {
            // adapter 事务尚未开始；沿真实释放路径归还已取得的执行权。
            self.finish_leased(lease);
            Err(AbortReason::Timeout)
        } else {
            Ok(lease)
        }
    }

    #[must_use]
    pub fn new(clock: Arc<dyn RobotClock>, ctrl: Arc<HardRtCtrlInner>) -> Self {
        Self {
            clock,
            ctrl,
            class: AtomicU8::new(BusOpClass::Connect as u8),
            intent: AtomicU8::new(InstalledIntent::None as u8),
            seq: AtomicU64::new(0),
            lease_deadline_ns: AtomicU64::new(0),
            confirmation_trusted: AtomicBool::new(false),
            observed_comm_error: AtomicU8::new(0),
            tick_has_call: AtomicBool::new(false),
            tick_overdue: AtomicBool::new(false),
        }
    }

    /// 为每次 adapter 调用安装上下文；不得在调用途中换域。
    pub(crate) fn set_context(
        &self,
        class: BusOpClass,
        intent: InstalledIntent,
        seq: u64,
        deadline: MonoRawNs,
        confirmation_trusted: bool,
    ) {
        // 一个 tick 的较早调用越界不能被较晚调用的新 deadline 抹掉。
        if self.tick_has_call.load(Ordering::Relaxed)
            && self.clock.now().0 >= self.lease_deadline_ns.load(Ordering::Relaxed)
        {
            self.tick_overdue.store(true, Ordering::Relaxed);
        }
        self.tick_has_call.store(true, Ordering::Relaxed);
        self.class.store(class as u8, Ordering::Relaxed);
        self.intent.store(intent as u8, Ordering::Relaxed);
        self.seq.store(seq, Ordering::Relaxed);
        self.lease_deadline_ns.store(deadline.0, Ordering::Relaxed);
        self.confirmation_trusted
            .store(confirmation_trusted, Ordering::Relaxed);
    }

    pub(crate) fn begin_tick(&self) {
        self.tick_has_call.store(false, Ordering::Relaxed);
        self.tick_overdue.store(false, Ordering::Relaxed);
    }

    /// 无 IO 的 ACK/终态 tick 可以推进；有 IO 则本 tick 所有调用都必须未越界。
    pub(crate) fn tick_progress_allowed(&self) -> bool {
        !self.tick_has_call.load(Ordering::Relaxed)
            || (!self.tick_overdue.load(Ordering::Relaxed)
                && self.clock.now().0 < self.lease_deadline_ns.load(Ordering::Relaxed))
    }

    /// 每次 Confirm 完成后先消费事实，再判断证据可信；不依赖诊断是否丢失。
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
        if self.ctrl.authority().snapshot().serial_estop_pending {
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
        #[cfg(feature = "test-comm-fault-hook")]
        {
            let mut context = self.ctrl.trace_context();
            context.source = self.operation_class() as u64;
            crate::comm_fault_trace::comm_error(
                crate::comm_fault_trace::now(),
                context,
                kind.to_u8(),
            );
        }
        // 记录首个事实；后续成功或其它错误不覆盖本次已发现的 hygiene 失败。
        let _ = self.observed_comm_error.compare_exchange(
            0,
            kind.to_u8() + 1,
            Ordering::Release,
            Ordering::Relaxed,
        );
    }
}

impl LeasedPreemptProbe for SerialPreemptProbe {
    fn begin_leased(&self, class: BusOpClass) -> Result<BusLease, AbortReason> {
        if let Some(reason) = self.should_abort(class) {
            return Err(reason);
        }
        if let Some(special) = BusLease::special(class) {
            return Ok(special);
        }
        let intent = match self.intent.load(Ordering::Relaxed) {
            1 => NormalIoIntent::PeriodicRead,
            2 => NormalIoIntent::Motion,
            3 => NormalIoIntent::EnableStep(
                self.ctrl
                    .execution_slot()
                    .load()
                    .ok_or(AbortReason::Revoked)?,
            ),
            4 => NormalIoIntent::NonEnergizingControl(NonEnergizingControl::Connect),
            5 => NormalIoIntent::NonEnergizingControl(NonEnergizingControl::Disconnect),
            6 => NormalIoIntent::NonEnergizingControl(NonEnergizingControl::Disable),
            _ => return Err(AbortReason::Revoked),
        };
        #[cfg(feature = "test-comm-fault-hook")]
        let acquired = {
            let observed = self.ctrl.authority().acquire_normal_io_observed(intent);
            self.ctrl.trace_now(
                crate::comm_fault_trace::TraceKind::NormalAcquire,
                observed.failed_cas.unwrap_or(u64::MAX),
                u64::from(observed.outcome.is_ok()),
            );
            observed.outcome
        };
        #[cfg(not(feature = "test-comm-fault-hook"))]
        let acquired = self.ctrl.authority().acquire_normal_io(intent);
        let lease = acquired.map_err(|reason| match reason {
            Refusal::SerialEStopPending => AbortReason::Emergency,
            _ => AbortReason::Revoked,
        })?;
        let lease = match BusLease::with_normal(class, lease) {
            Ok(lease) => lease,
            Err(lease) => {
                // 内部 class/intent 错配不能伪装为一次可执行事务。
                #[cfg(feature = "test-comm-fault-hook")]
                {
                    let observed = self.ctrl.authority().release_normal_io_observed(lease);
                    self.ctrl.trace_now(
                        crate::comm_fault_trace::TraceKind::NormalRelease,
                        observed.failed_cas.unwrap_or(u64::MAX),
                        match observed.outcome {
                            Ok(_) => 1,
                            Err(_) => 3,
                        },
                    );
                }
                #[cfg(not(feature = "test-comm-fault-hook"))]
                let _ = self.ctrl.authority().release_normal_io(lease);
                return Err(AbortReason::Revoked);
            }
        };
        #[cfg(feature = "test-comm-fault-hook")]
        {
            let mut context = self.ctrl.trace_context();
            context.source = class as u64;
            self.finish_injected_delay(
                lease,
                crate::comm_fault_hook::delay_acquired_normal(context),
            )
        }
        #[cfg(not(feature = "test-comm-fault-hook"))]
        Ok(lease)
    }

    fn finish_leased(&self, lease: BusLease) {
        let normal = lease.holds_normal_lease();
        #[cfg(feature = "test-comm-fault-hook")]
        {
            let observed = lease.release_observed(self.ctrl.authority());
            if normal {
                self.ctrl.trace_now(
                    crate::comm_fault_trace::TraceKind::NormalRelease,
                    observed.failed_cas.unwrap_or(u64::MAX),
                    match observed.outcome {
                        Ok(_) => 1,
                        Err(_) => 3,
                    },
                );
            }
        }
        #[cfg(not(feature = "test-comm-fault-hook"))]
        let _ = (normal, lease.release(self.ctrl.authority()));
        // finish_leased 的既批签名为 ()；失败不清权限位，也不冒称返给 adapter。
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hard_rt_ctrl::{HardRtCtrlClient, NORMAL_IO_ACTIVE};
    use robot_core::executor_ctrl::{ExecutorCtrlClient, SerialEmergencyCtrl};
    use robot_core::time::MockClock;

    #[cfg(feature = "test-comm-fault-hook")]
    #[test]
    fn injected_delay_error_releases_active_lease_with_real_trace() {
        let ctrl = Arc::new(HardRtCtrlInner::new_serial(
            crate::hard_rt_ctrl::test_authority(false),
        ));
        let probe = SerialPreemptProbe::new(Arc::new(MockClock::default()), ctrl.clone());
        probe.set_context(
            BusOpClass::NormalEffectful,
            InstalledIntent::Disable,
            9,
            MonoRawNs(100),
            false,
        );
        let events = crate::comm_fault_trace::capture_emitted(|| {
            let lease = probe.begin_leased(BusOpClass::NormalEffectful).unwrap();
            assert_ne!(ctrl.safety_word() & NORMAL_IO_ACTIVE, 0);
            assert_eq!(
                probe
                    .finish_injected_delay(
                        lease,
                        Err(crate::comm_fault_hook::DelayFailure::Incomplete)
                    )
                    .unwrap_err(),
                AbortReason::Timeout
            );
            assert_eq!(ctrl.safety_word() & NORMAL_IO_ACTIVE, 0);
        });
        assert!(events.iter().any(|event| event.kind
            == crate::comm_fault_trace::TraceKind::NormalRelease
            && event.b == 1));
    }

    #[test]
    fn transaction_release_preserves_emergency_and_checkpoint_keeps_lease() {
        let clock = Arc::new(MockClock::default());
        let ctrl = Arc::new(HardRtCtrlInner::new_serial(
            crate::hard_rt_ctrl::test_authority(false),
        ));
        let client = HardRtCtrlClient::new(Arc::clone(&ctrl));
        let probe = SerialPreemptProbe::new(clock.clone(), ctrl.clone());
        probe.set_context(
            BusOpClass::NormalEffectful,
            crate::serial_preempt::InstalledIntent::Disable,
            9,
            MonoRawNs(100),
            false,
        );
        let lease = probe.begin_leased(BusOpClass::NormalEffectful).unwrap();
        probe.checkpoint();
        assert_ne!(ctrl.safety_word() & NORMAL_IO_ACTIVE, 0);
        client.estop_monotonic();
        probe.finish_leased(lease);
        assert_eq!(ctrl.safety_word() & NORMAL_IO_ACTIVE, 0);
        assert_eq!(
            probe.begin_leased(BusOpClass::PeriodicRead).unwrap_err(),
            AbortReason::Emergency
        );
        let safety = probe.begin_leased(BusOpClass::Safety).unwrap();
        probe.finish_leased(safety);
        assert_eq!(ctrl.liveness_counter.load(Ordering::Relaxed), 1);
        clock.set(MonoRawNs(100));
        probe.checkpoint();
        assert_eq!(ctrl.liveness_counter.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn matching_timeout_and_sliced_sleep_use_the_correct_operation_domain() {
        let clock = Arc::new(MockClock::default());
        let ctrl = Arc::new(HardRtCtrlInner::new_serial(
            crate::hard_rt_ctrl::test_authority(false),
        ));
        let client = HardRtCtrlClient::new(ctrl.clone());
        let probe = SerialPreemptProbe::new(clock.clone(), ctrl.clone());
        probe.set_context(
            BusOpClass::NormalEffectful,
            crate::serial_preempt::InstalledIntent::Disable,
            9,
            MonoRawNs(100_000_000),
            true,
        );
        client.abort_pending_normal(NormalAbortTarget::Submitted { seq: 8 });
        assert_eq!(
            probe.sleep_or_abort(BusOpClass::NormalEffectful, DurationNs(12_000_000)),
            Ok(())
        );
        assert_eq!(clock.now(), MonoRawNs(12_000_000));
        assert_eq!(ctrl.safety_word() & NORMAL_IO_ACTIVE, 0);
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
            Arc::new(HardRtCtrlInner::new_serial(
                crate::hard_rt_ctrl::test_authority(false),
            )),
        );
        assert_eq!(probe.take_comm_error(), None);
        probe.report_comm_error(CommErrorKind::DiscardFailed);
        probe.report_comm_error(CommErrorKind::FrameCorrupt);
        assert_eq!(probe.take_comm_error(), Some(CommErrorKind::DiscardFailed));
        assert_eq!(probe.take_comm_error(), None);
    }

    #[test]
    fn tick_progress_preserves_an_earlier_overrun_but_recovers_for_a_no_io_tick() {
        let clock = Arc::new(MockClock::default());
        let probe = SerialPreemptProbe::new(
            clock.clone(),
            Arc::new(HardRtCtrlInner::new_serial(
                crate::hard_rt_ctrl::test_authority(false),
            )),
        );
        probe.begin_tick();
        probe.set_context(
            BusOpClass::NormalEffectful,
            crate::serial_preempt::InstalledIntent::Disable,
            1,
            MonoRawNs(100),
            true,
        );
        clock.set(MonoRawNs(101));
        probe.set_context(
            BusOpClass::PeriodicRead,
            crate::serial_preempt::InstalledIntent::PeriodicRead,
            0,
            MonoRawNs(1_000),
            true,
        );
        assert!(
            !probe.tick_progress_allowed(),
            "后续新预算不能洗掉同tick较早调用的越界"
        );
        probe.begin_tick();
        assert!(
            probe.tick_progress_allowed(),
            "无IO的对账等待不继承过期lease"
        );
        probe.set_context(
            BusOpClass::PeriodicRead,
            crate::serial_preempt::InstalledIntent::PeriodicRead,
            0,
            MonoRawNs(200),
            true,
        );
        clock.set(MonoRawNs(199));
        assert!(probe.tick_progress_allowed());
        clock.set(MonoRawNs(200));
        assert!(
            !probe.tick_progress_allowed(),
            "与checkpoint共用严格未到期条件"
        );
    }
}
