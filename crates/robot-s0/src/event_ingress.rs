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
