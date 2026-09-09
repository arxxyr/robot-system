//! 类型化的有界输入 lane；急停意图直接进入原子协议，不排普通事件队列。

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use robot_core::executor_ctrl::{ExecutorCtrlClient, OutputPolicy, SerialEmergencyCtrl};

use crate::hard_rt_ctrl::MailboxSnapshot;
use crate::ipc_thread::{ClearEStopRrRequest, SetEnabledRrRequest};
use crate::state_machine::{Event, FaultCode};

pub const RR_CHANNEL_CAPACITY: usize = 64;
pub const LINK_CAPACITY: usize = 64;
pub const RR_CAPACITY: usize = 128;
pub const CONTROL_CAPACITY: usize = 128;
pub const EXTERNAL_CHANNEL_CAPACITY: usize = 64;
pub const SOURCE_QUOTA: usize = 8;
pub const LINK_QUOTA: usize = 16;
pub const RR_QUOTA: usize = 24;
pub const CONTROL_QUOTA: usize = 24;
const _: () = assert!(RR_CAPACITY == 2 * RR_CHANNEL_CAPACITY);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkEvent {
    Heartbeat,
    HeartbeatTimeout,
}

impl From<LinkEvent> for Event {
    fn from(event: LinkEvent) -> Self {
        match event {
            LinkEvent::Heartbeat => Self::Heartbeat,
            LinkEvent::HeartbeatTimeout => Self::HeartbeatTimeout,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlEvent {
    Enable,
    Disable,
    ClearError,
    ClearEStop,
    Stop,
    AdapterErr(FaultCode),
    CmdDeadlineExpired,
    LimitViolation,
    WatchdogTimeout,
    RtOverrun,
}

impl From<ControlEvent> for Event {
    fn from(event: ControlEvent) -> Self {
        match event {
            ControlEvent::Enable => Self::Enable,
            ControlEvent::Disable => Self::Disable,
            ControlEvent::ClearError => Self::ClearError,
            ControlEvent::ClearEStop => Self::ClearEStop,
            ControlEvent::Stop => Self::Stop,
            ControlEvent::AdapterErr(code) => Self::AdapterErr(code),
            ControlEvent::CmdDeadlineExpired => Self::CmdDeadlineExpired,
            ControlEvent::LimitViolation => Self::LimitViolation,
            ControlEvent::WatchdogTimeout => Self::WatchdogTimeout,
            ControlEvent::RtOverrun => Self::RtOverrun,
        }
    }
}

impl TryFrom<Event> for ControlEvent {
    type Error = Event;

    fn try_from(event: Event) -> Result<Self, Self::Error> {
        Ok(match event {
            Event::Enable => Self::Enable,
            Event::Disable => Self::Disable,
            Event::ClearError => Self::ClearError,
            Event::ClearEStop => Self::ClearEStop,
            Event::Stop => Self::Stop,
            Event::AdapterErr(code) => Self::AdapterErr(code),
            Event::CmdDeadlineExpired => Self::CmdDeadlineExpired,
            Event::LimitViolation => Self::LimitViolation,
            Event::WatchdogTimeout => Self::WatchdogTimeout,
            Event::RtOverrun => Self::RtOverrun,
            Event::Heartbeat | Event::HeartbeatTimeout | Event::EStop => return Err(event),
        })
    }
}

#[derive(Debug)]
pub enum RrEvent {
    ClearEStop(ClearEStopRrRequest),
    SetEnabled(SetEnabledRrRequest),
}

impl RrEvent {
    #[must_use]
    pub const fn deadline_ns(&self) -> u64 {
        match self {
            Self::ClearEStop(request) => request.deadline_ns,
            Self::SetEnabled(request) => request.deadline_ns,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmitError {
    Full,
    Disconnected,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct EmergencyBatch {
    pub estop: bool,
    pub ssf: Option<MailboxSnapshot>,
}

/// 调用者返回之后，急停仍有独立的 serial 与 IPC 消费义务。
#[derive(Clone)]
pub struct ExternalEvents {
    control: Sender<ControlEvent>,
    legacy_notice: Arc<AtomicBool>,
    executor: Arc<dyn ExecutorCtrlClient>,
    emergency: Option<Arc<dyn SerialEmergencyCtrl>>,
}

impl ExternalEvents {
    pub(crate) fn channel(
        legacy_notice: Arc<AtomicBool>,
        executor: Arc<dyn ExecutorCtrlClient>,
        emergency: Option<Arc<dyn SerialEmergencyCtrl>>,
    ) -> (Self, Receiver<ControlEvent>) {
        let (control, receiver) = bounded(EXTERNAL_CHANNEL_CAPACITY);
        (
            Self {
                control,
                legacy_notice,
                executor,
                emergency,
            },
            receiver,
        )
    }

    pub fn estop(&self) {
        #[cfg(feature = "test-comm-fault-hook")]
        crate::comm_fault_trace::emit(
            crate::comm_fault_trace::TraceKind::ExternalEnter,
            crate::comm_fault_trace::now(),
            crate::comm_fault_trace::TraceContext::default(),
            0,
            0,
        );
        if let Some(emergency) = &self.emergency {
            let outcome = emergency.estop_monotonic();
            tracing::info!(?outcome, "外部急停原语已发布");
        } else {
            self.legacy_notice.store(true, Ordering::Release);
            self.executor.safety_stop();
        }
    }

    /// 普通控制入口在类型上不接受急停。
    ///
    /// # Errors
    ///
    /// 队列满或消费者退出立即返回；Stop 即使入队失败也已切 Disabled。
    pub fn control(&self, event: ControlEvent) -> Result<(), AdmitError> {
        if event == ControlEvent::Stop {
            self.executor.set_policy(OutputPolicy::Disabled);
        }
        self.control.try_send(event).map_err(|error| match error {
            TrySendError::Full(_) => AdmitError::Full,
            TrySendError::Disconnected(_) => AdmitError::Disconnected,
        })
    }
}

/// 队列在构造时预分配，全部 admission 在 push 之前检查容量。
#[derive(Debug)]
pub struct EventQueue {
    link: VecDeque<LinkEvent>,
    rr: VecDeque<RrEvent>,
    rr_estop_prefix: usize,
    control: VecDeque<ControlEvent>,
    estop: Arc<AtomicBool>,
    legacy_ssf: bool,
    link_overflow: bool,
    overflow_replayed: u64,
}

impl Default for EventQueue {
    fn default() -> Self {
        Self::with_estop_notice(Arc::new(AtomicBool::new(false)))
    }
}

impl EventQueue {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn with_estop_notice(estop: Arc<AtomicBool>) -> Self {
        Self {
            link: VecDeque::with_capacity(LINK_CAPACITY),
            rr: VecDeque::with_capacity(RR_CAPACITY),
            rr_estop_prefix: 0,
            control: VecDeque::with_capacity(CONTROL_CAPACITY),
            estop,
            legacy_ssf: false,
            link_overflow: false,
            overflow_replayed: 0,
        }
    }

    pub fn admit_link(&mut self, event: LinkEvent) {
        if self.link.back() == Some(&event) {
            return;
        }
        if self.link.len() == LINK_CAPACITY {
            self.link_overflow = true;
        } else {
            self.link.push_back(event);
        }
    }

    /// # Errors
    ///
    /// Full 时归还请求所有权，由 admission 调用者生成唯一失败响应。
    pub fn admit_rr(&mut self, event: RrEvent) -> Result<(), RrEvent> {
        if self.rr.len() == RR_CAPACITY {
            return Err(event);
        }
        self.rr.push_back(event);
        Ok(())
    }

    /// # Errors
    ///
    /// 普通 lane 已满返回 Full，连续重复无需增加存储。
    pub fn admit_control(&mut self, event: ControlEvent) -> Result<(), AdmitError> {
        if self.control.back() == Some(&event) {
            return Ok(());
        }
        if self.control.len() == CONTROL_CAPACITY {
            return Err(AdmitError::Full);
        }
        self.control.push_back(event);
        Ok(())
    }

    pub fn pop_link(&mut self) -> Option<LinkEvent> {
        match self.link.pop_front() {
            Some(event) => Some(event),
            None if self.link_overflow => {
                self.link_overflow = false;
                self.overflow_replayed += 1;
                Some(LinkEvent::HeartbeatTimeout)
            }
            None => None,
        }
    }

    pub fn pop_rr(&mut self) -> Option<RrEvent> {
        self.pop_rr_with_preemption().map(|(request, _)| request)
    }

    pub(crate) fn mark_rr_estop_prefix(&mut self) {
        self.rr_estop_prefix = self.rr.len();
    }

    pub(crate) fn pop_rr_with_preemption(&mut self) -> Option<(RrEvent, bool)> {
        let request = self.rr.pop_front()?;
        let preempted = self.rr_estop_prefix != 0;
        self.rr_estop_prefix = self.rr_estop_prefix.saturating_sub(1);
        Some((request, preempted))
    }

    pub fn pop_control(&mut self) -> Option<ControlEvent> {
        self.control.pop_front()
    }

    pub fn set_estop(&self) {
        self.estop.store(true, Ordering::Release);
    }

    #[must_use]
    pub fn claim_estop(&self) -> bool {
        self.estop.swap(false, Ordering::AcqRel)
    }

    #[must_use]
    pub const fn overflow_replayed(&self) -> u64 {
        self.overflow_replayed
    }

    #[must_use]
    pub fn lane_lengths(&self) -> (usize, usize, usize) {
        (self.link.len(), self.rr.len(), self.control.len())
    }

    /// 既有状态机测试的事件入口，与生产 admission 共用同一组有界 lane。
    pub fn push(&mut self, event: Event) {
        match event {
            Event::EStop => self.set_estop(),
            Event::AdapterErr(FaultCode::SafetyStopFailed) => self.legacy_ssf = true,
            Event::Heartbeat => self.admit_link(LinkEvent::Heartbeat),
            Event::HeartbeatTimeout => self.admit_link(LinkEvent::HeartbeatTimeout),
            event => {
                if let Ok(control) = ControlEvent::try_from(event) {
                    let _ = self.admit_control(control);
                }
            }
        }
    }

    pub fn pop(&mut self) -> Option<Event> {
        self.pop_link()
            .map(Event::from)
            .or_else(|| self.pop_control().map(Event::from))
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.link.len()
            + self.rr.len()
            + self.control.len()
            + usize::from(self.estop.load(Ordering::Acquire))
            + usize::from(self.legacy_ssf)
            + usize::from(self.link_overflow)
    }

    pub fn drain_emergency(&mut self) -> Option<Event> {
        match () {
            () if self.claim_estop() => Some(Event::EStop),
            () if std::mem::take(&mut self.legacy_ssf) => {
                Some(Event::AdapterErr(FaultCode::SafetyStopFailed))
            }
            () => None,
        }
    }

    #[must_use]
    pub fn has_emergency(&self) -> bool {
        self.estop.load(Ordering::Acquire) || self.legacy_ssf
    }

    pub(crate) fn claim_legacy_ssf(&mut self) -> bool {
        std::mem::take(&mut self.legacy_ssf)
    }

    pub fn remove_clear_estop_events(&mut self) -> usize {
        let before = self.control.len();
        self.control
            .retain(|event| *event != ControlEvent::ClearEStop);
        before - self.control.len()
    }
}

/// 高优先级工作保留真实请求的唯一所有权。
#[derive(Debug)]
pub enum HighWork {
    LiveDisable(SetEnabledRrRequest),
    Control(ControlEvent),
    LiveEnable(SetEnabledRrRequest),
    LiveClear(ClearEStopRrRequest),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HighClass {
    Lower,
    Restore,
}

impl HighWork {
    pub fn from_rr(event: RrEvent) -> Result<Self, RrEvent> {
        match event {
            RrEvent::SetEnabled(request) if !request.dry_run => {
                if request.enabled {
                    Ok(Self::LiveEnable(request))
                } else {
                    Ok(Self::LiveDisable(request))
                }
            }
            RrEvent::ClearEStop(request) if !request.dry_run => Ok(Self::LiveClear(request)),
            event => Err(event),
        }
    }

    #[must_use]
    pub const fn control(event: ControlEvent) -> Self {
        Self::Control(event)
    }

    #[must_use]
    pub const fn class(&self) -> HighClass {
        match self {
            Self::LiveDisable(_)
            | Self::Control(
                ControlEvent::Stop
                | ControlEvent::Disable
                | ControlEvent::AdapterErr(_)
                | ControlEvent::CmdDeadlineExpired
                | ControlEvent::LimitViolation
                | ControlEvent::WatchdogTimeout
                | ControlEvent::RtOverrun,
            ) => HighClass::Lower,
            Self::LiveEnable(_)
            | Self::LiveClear(_)
            | Self::Control(
                ControlEvent::Enable | ControlEvent::ClearEStop | ControlEvent::ClearError,
            ) => HighClass::Restore,
        }
    }
}

#[derive(Debug)]
pub enum NormalWork {
    DryRunSet(SetEnabledRrRequest),
    DryRunClear(ClearEStopRrRequest),
}

impl NormalWork {
    pub fn from_rr(event: RrEvent) -> Result<Self, RrEvent> {
        match event {
            RrEvent::SetEnabled(request) if request.dry_run => Ok(Self::DryRunSet(request)),
            RrEvent::ClearEStop(request) if request.dry_run => Ok(Self::DryRunClear(request)),
            event => Err(event),
        }
    }
}

pub const HIGH_QUEUE_CAPACITY: usize = 32;

/// 两个类别分别预留全部容量，合计准入上限仍为 32。
#[derive(Debug)]
pub struct HighQueue {
    lower: VecDeque<HighWork>,
    restore: VecDeque<HighWork>,
}

impl Default for HighQueue {
    fn default() -> Self {
        Self {
            lower: VecDeque::with_capacity(HIGH_QUEUE_CAPACITY),
            restore: VecDeque::with_capacity(HIGH_QUEUE_CAPACITY),
        }
    }
}

impl HighQueue {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn admit(&mut self, work: HighWork) -> Result<(), HighWork> {
        if self.len() == HIGH_QUEUE_CAPACITY {
            return Err(work);
        }
        match work.class() {
            HighClass::Lower => self.lower.push_back(work),
            HighClass::Restore => self.restore.push_back(work),
        }
        Ok(())
    }

    pub fn pop(&mut self) -> Option<HighWork> {
        self.pop_lower().or_else(|| self.pop_restore())
    }
    pub fn pop_lower(&mut self) -> Option<HighWork> {
        self.lower.pop_front()
    }
    pub fn pop_restore(&mut self) -> Option<HighWork> {
        self.restore.pop_front()
    }
    #[must_use]
    pub fn len(&self) -> usize {
        self.lower.len() + self.restore.len()
    }
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    #[must_use]
    pub fn len_lower(&self) -> usize {
        self.lower.len()
    }
    #[must_use]
    pub fn len_restore(&self) -> usize {
        self.restore.len()
    }
}

/// 两个 RR 来源共用的普通工作队列；容量在非 RT 初始化时预分配。
#[derive(Debug)]
pub struct NormalQueue {
    items: VecDeque<NormalWork>,
    cap: usize,
}

impl NormalQueue {
    #[must_use]
    pub const fn capacity_for(limits: &crate::rr_admission::CheckedLimits) -> usize {
        2 * crate::rr_admission::ClassCaps::from_checked(limits).normal
    }

    #[must_use]
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            items: VecDeque::with_capacity(cap),
            cap,
        }
    }
    pub fn admit(&mut self, work: NormalWork) -> Result<(), NormalWork> {
        if self.items.len() == self.cap {
            return Err(work);
        }
        self.items.push_back(work);
        Ok(())
    }
    pub fn pop(&mut self) -> Option<NormalWork> {
        self.items.pop_front()
    }
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.cap
    }
}

#[cfg(test)]
mod priority_tests {
    use super::*;
    use crate::rr_admission::CheckedLimits;
    use robot_transport::iceoryx2::RrStaticLimits;

    fn set(id: u64, dry_run: bool, enabled: bool) -> RrEvent {
        RrEvent::SetEnabled(SetEnabledRrRequest {
            request_id: id.to_string(),
            enabled,
            dry_run,
            deadline_ns: id,
            audit_reason: "准入测试".into(),
        })
    }
    fn clear(id: u64, dry_run: bool) -> RrEvent {
        RrEvent::ClearEStop(ClearEStopRrRequest {
            request_id: id.to_string(),
            dry_run,
            deadline_ns: id,
            confirmation: "确认测试".into(),
            audit_reason: "准入测试".into(),
        })
    }
    fn normal_capacity(c: usize) -> usize {
        NormalQueue::capacity_for(
            &CheckedLimits::try_from_limits(&RrStaticLimits { c, l: 4 }).unwrap(),
        )
    }
    fn assert_set(event: RrEvent, id: u64, dry_run: bool, enabled: bool) {
        let RrEvent::SetEnabled(request) = event else {
            panic!("必须返原 Set 请求")
        };
        assert_eq!(request.request_id, id.to_string());
        assert_eq!(request.deadline_ns, id);
        assert_eq!(request.dry_run, dry_run);
        assert_eq!(request.enabled, enabled);
        assert_eq!(request.audit_reason, "准入测试");
    }
    fn assert_clear(event: RrEvent, id: u64, dry_run: bool) {
        let RrEvent::ClearEStop(request) = event else {
            panic!("必须返原 Clear 请求")
        };
        assert_eq!(request.request_id, id.to_string());
        assert_eq!(request.deadline_ns, id);
        assert_eq!(request.dry_run, dry_run);
        assert_eq!(request.audit_reason, "准入测试");
        assert_eq!(request.confirmation, "确认测试");
    }

    #[test]
    fn high_admits_real_three_sources_24_per_tick_and_drains() {
        let mut queue = HighQueue::new();
        for tick in 0..32 {
            for offset in 0..SOURCE_QUOTA {
                let id = u64::try_from(tick * SOURCE_QUOTA + offset).unwrap();
                queue.admit(HighWork::control(ControlEvent::Stop)).unwrap();
                queue
                    .admit(HighWork::from_rr(clear(id, false)).unwrap())
                    .unwrap();
                queue
                    .admit(HighWork::from_rr(set(id, false, offset % 2 == 0)).unwrap())
                    .unwrap();
            }
            assert_eq!(queue.len(), 24);
            let (mut external, mut clears, mut sets) = (0, 0, 0);
            while let Some(work) = queue.pop() {
                match work {
                    HighWork::Control(_) => external += 1,
                    HighWork::LiveClear(_) => clears += 1,
                    HighWork::LiveEnable(_) | HighWork::LiveDisable(_) => sets += 1,
                }
            }
            assert_eq!((external, clears, sets), (8, 8, 8));
            assert!(queue.is_empty());
        }
    }

    #[test]
    fn high_pop_order_lower_before_restore_fifo_within_class() {
        let mut queue = HighQueue::new();
        queue
            .admit(HighWork::from_rr(set(1, false, true)).unwrap())
            .unwrap();
        queue.admit(HighWork::control(ControlEvent::Stop)).unwrap();
        queue
            .admit(HighWork::from_rr(clear(2, false)).unwrap())
            .unwrap();
        queue
            .admit(HighWork::from_rr(set(3, false, false)).unwrap())
            .unwrap();
        assert_eq!((queue.len_lower(), queue.len_restore()), (2, 2));
        assert!(matches!(
            queue.pop(),
            Some(HighWork::Control(ControlEvent::Stop))
        ));
        let Some(HighWork::LiveDisable(request)) = queue.pop() else {
            panic!("第二条必须 Lower FIFO")
        };
        assert_set(RrEvent::SetEnabled(request), 3, false, false);
        let Some(HighWork::LiveEnable(request)) = queue.pop() else {
            panic!("第三条必须 Restore FIFO")
        };
        assert_set(RrEvent::SetEnabled(request), 1, false, true);
        let Some(HighWork::LiveClear(request)) = queue.pop() else {
            panic!("第四条必须 Clear")
        };
        assert_clear(RrEvent::ClearEStop(request), 2, false);
        assert!(queue.pop().is_none());
    }

    #[test]
    fn high_33rd_returns_original_payload() {
        let mut queue = HighQueue::new();
        for _ in 0..HIGH_QUEUE_CAPACITY {
            queue
                .admit(HighWork::control(ControlEvent::Enable))
                .unwrap();
        }
        let rejected = queue
            .admit(HighWork::from_rr(set(33, false, false)).unwrap())
            .unwrap_err();
        let HighWork::LiveDisable(request) = rejected else {
            panic!("必须归还 Disable 原请求")
        };
        assert_set(RrEvent::SetEnabled(request), 33, false, false);
        assert_eq!(
            (queue.len(), queue.len_lower(), queue.len_restore()),
            (32, 0, 32)
        );
        assert!(queue.pop_lower().is_none());
        assert!(queue.pop_restore().is_some());
    }

    #[test]
    fn high_work_from_rr_rejects_dry_run_returning_event() {
        for enabled in [false, true] {
            assert_set(
                HighWork::from_rr(set(10, true, enabled)).unwrap_err(),
                10,
                true,
                enabled,
            );
        }
        assert_clear(HighWork::from_rr(clear(11, true)).unwrap_err(), 11, true);
    }

    #[test]
    fn normal_work_from_rr_rejects_live_returning_event() {
        for enabled in [false, true] {
            assert_set(
                NormalWork::from_rr(set(10, false, enabled)).unwrap_err(),
                10,
                false,
                enabled,
            );
        }
        assert_clear(
            NormalWork::from_rr(clear(11, false)).unwrap_err(),
            11,
            false,
        );
    }

    #[test]
    fn control_event_class_table() {
        for (event, class) in [
            (ControlEvent::Enable, HighClass::Restore),
            (ControlEvent::Disable, HighClass::Lower),
            (ControlEvent::ClearError, HighClass::Restore),
            (ControlEvent::ClearEStop, HighClass::Restore),
            (ControlEvent::Stop, HighClass::Lower),
            (
                ControlEvent::AdapterErr(FaultCode::SafetyStopFailed),
                HighClass::Lower,
            ),
            (ControlEvent::CmdDeadlineExpired, HighClass::Lower),
            (ControlEvent::LimitViolation, HighClass::Lower),
            (ControlEvent::WatchdogTimeout, HighClass::Lower),
            (ControlEvent::RtOverrun, HighClass::Lower),
        ] {
            assert_eq!(HighWork::control(event).class(), class);
        }
    }

    #[test]
    fn normal_capacity_for_c8_l4_is_32_and_c16_is_64() {
        assert_eq!(normal_capacity(8), 32);
        assert_eq!(normal_capacity(16), 64);
    }

    #[test]
    fn normal_65th_returns_original_payload_under_c16() {
        let mut queue = NormalQueue::with_capacity(normal_capacity(16));
        for id in 0..64 {
            queue
                .admit(NormalWork::from_rr(set(id, true, id % 2 == 0)).unwrap())
                .unwrap();
        }
        let NormalWork::DryRunClear(request) = queue
            .admit(NormalWork::from_rr(clear(65, true)).unwrap())
            .unwrap_err()
        else {
            panic!("必须归还 Clear 原请求")
        };
        assert_clear(RrEvent::ClearEStop(request), 65, true);
        assert_eq!((queue.len(), queue.capacity()), (64, 64));
        for id in 0..64 {
            let Some(NormalWork::DryRunSet(request)) = queue.pop() else {
                panic!("普通 FIFO 缺请求")
            };
            assert_set(RrEvent::SetEnabled(request), id, true, id % 2 == 0);
        }
        assert!(queue.is_empty());
    }

    #[test]
    fn full_high_channel_of_2c_needs_ceil_2c_over_8_ticks() {
        for (c, expected_ticks) in [(8, 2), (16, 4)] {
            let (tx, rx) = bounded(2 * c);
            for id in 0..2 * c {
                tx.try_send(
                    HighWork::from_rr(set(u64::try_from(id).unwrap(), false, false)).unwrap(),
                )
                .unwrap();
            }
            let mut queue = HighQueue::new();
            let mut ticks = 0;
            let mut drained = 0;
            while !rx.is_empty() {
                ticks += 1;
                for _ in 0..SOURCE_QUOTA {
                    match rx.try_recv() {
                        Ok(work) => queue.admit(work).unwrap(),
                        Err(_) => break,
                    }
                }
                while queue.pop().is_some() {
                    drained += 1;
                }
            }
            assert_eq!(ticks, expected_ticks);
            assert_eq!(drained, 2 * c);
        }
    }
}
