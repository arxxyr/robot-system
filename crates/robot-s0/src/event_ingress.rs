//! 类型化的有界输入 lane；急停意图直接进入原子协议，不排普通事件队列。

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use robot_core::executor_ctrl::{ExecutorCtrlClient, OutputPolicy, SerialEmergencyCtrl};

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

/// 链路事实按原序回放；相邻重复合并，满载后最后追加一次保守 Timeout。
#[derive(Debug)]
pub(crate) struct LinkQueue {
    events: VecDeque<LinkEvent>,
    overflow: bool,
    replayed: u64,
}
impl LinkQueue {
    pub(crate) fn new() -> Self {
        Self {
            events: VecDeque::with_capacity(LINK_CAPACITY),
            overflow: false,
            replayed: 0,
        }
    }
    pub(crate) fn admit(&mut self, event: LinkEvent) {
        if self.events.back() == Some(&event) {
            return;
        }
        if self.events.len() == LINK_CAPACITY {
            self.overflow = true;
        } else {
            self.events.push_back(event);
        }
    }
    pub(crate) fn pop(&mut self) -> Option<LinkEvent> {
        match self.events.pop_front() {
            Some(event) => Some(event),
            None if self.overflow => {
                self.overflow = false;
                self.replayed = self.replayed.saturating_add(1);
                Some(LinkEvent::HeartbeatTimeout)
            }
            None => None,
        }
    }
    pub(crate) fn clear(&mut self) {
        self.events.clear();
        self.overflow = false;
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.events.is_empty() && !self.overflow
    }
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.events.len() + usize::from(self.overflow)
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
    restore_estop_prefix: usize,
}

impl Default for HighQueue {
    fn default() -> Self {
        Self {
            lower: VecDeque::with_capacity(HIGH_QUEUE_CAPACITY),
            restore: VecDeque::with_capacity(HIGH_QUEUE_CAPACITY),
            restore_estop_prefix: 0,
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
        self.pop_lower()
            .or_else(|| self.pop_restore().map(|(work, _)| work))
    }
    pub fn pop_lower(&mut self) -> Option<HighWork> {
        self.lower.pop_front()
    }
    pub fn pop_restore(&mut self) -> Option<(HighWork, bool)> {
        let work = self.restore.pop_front()?;
        let preempted = self.restore_estop_prefix != 0;
        self.restore_estop_prefix = self.restore_estop_prefix.saturating_sub(1);
        Some((work, preempted))
    }
    /// 只在 IPC 真正消费急停公告时标记当时队列前缀。
    pub(crate) fn mark_restore_estop_prefix(&mut self) {
        self.restore_estop_prefix = self.restore.len();
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
            // 这里只测试有界队列的 POD 路由转发，不存在传输层在飞 RR。
            route: crate::test_util::TestRrRoute::new().identity(),
            request_id: id.to_string(),
            enabled,
            dry_run,
            deadline_ns: id,
            audit_reason: "准入测试".into(),
        })
    }
    fn clear(id: u64, dry_run: bool) -> RrEvent {
        RrEvent::ClearEStop(ClearEStopRrRequest {
            route: crate::test_util::TestRrRoute::new().identity(),
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
