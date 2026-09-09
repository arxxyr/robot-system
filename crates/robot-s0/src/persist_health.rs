//! 持久化健康的当前真值与历史沿分离；不改变 writer、义务或 Clear 责任。

use crate::clear_coordination::StuckReason;
use crate::latch_writer::{ObligationView, obligation_overdue};
use robot_core::time::{DurationNs, MonoRawNs};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HealthInputs {
    pub obligation: ObligationView,
    pub now_ns: u64,
    pub alarm_ns: u64,
    pub writer_alive: bool,
    pub clear_stuck: Option<StuckReason>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CurrentDegradation {
    PersistPending,
    WriterGone,
    RepairExhausted,
    MarkerLost,
    ProtocolBug,
    PairMismatch,
    RetireRefused,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnsettledView {
    pub since_ns: u64,
    pub overdue: bool,
}

#[derive(Debug, Eq, PartialEq)]
pub struct HealthReport {
    pub current: Vec<CurrentDegradation>,
    pub unsettled: Option<UnsettledView>,
}

/// 普通线程的纯计算；三个来源各自判定，互不遮蔽。
#[must_use]
pub fn classify(inputs: HealthInputs) -> HealthReport {
    let overdue = obligation_overdue(
        &inputs.obligation,
        MonoRawNs(inputs.now_ns),
        DurationNs(inputs.alarm_ns),
    );
    let unsettled = if inputs.obligation.is_settled() {
        None
    } else {
        inputs
            .obligation
            .unsettled_since
            .map(|since| UnsettledView {
                since_ns: since.0,
                overdue,
            })
    };
    let mut current = Vec::with_capacity(3);
    if overdue {
        current.push(CurrentDegradation::PersistPending);
    }
    if !inputs.writer_alive {
        current.push(CurrentDegradation::WriterGone);
    }
    match inputs.clear_stuck {
        Some(StuckReason::RepairExhausted) => current.push(CurrentDegradation::RepairExhausted),
        Some(StuckReason::MarkerLost) => current.push(CurrentDegradation::MarkerLost),
        Some(StuckReason::ProtocolBug(_)) => current.push(CurrentDegradation::ProtocolBug),
        Some(StuckReason::PairMismatch) => current.push(CurrentDegradation::PairMismatch),
        Some(StuckReason::RetireRefused(_)) => current.push(CurrentDegradation::RetireRefused),
        // WriterGone 的当前真值唯一来自连接事实，不能由旧 txn 状态补造。
        Some(StuckReason::WriterGone) | None => {}
    }
    HealthReport { current, unsettled }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StickyEdge {
    pub since_ns: u64,
}

#[derive(Debug, Default)]
pub struct OverdueTracker {
    first: Option<StickyEdge>,
}
impl OverdueTracker {
    #[must_use]
    pub const fn new() -> Self {
        Self { first: None }
    }
    #[must_use]
    pub const fn first(&self) -> Option<StickyEdge> {
        self.first
    }

    /// 全生命周期仅一次沿；settled 与新一轮超期都不重置历史。
    pub fn observe(&mut self, report: &HealthReport) -> Option<StickyEdge> {
        if self.first.is_some() {
            return None;
        }
        let unsettled = report.unsettled.filter(|view| view.overdue)?;
        let edge = StickyEdge {
            since_ns: unsettled.since_ns,
        };
        self.first = Some(edge);
        Some(edge)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clear_coordination::ProtocolFault;
    use robot_core::control_authority::Refusal;

    fn inputs(now_ns: u64) -> HealthInputs {
        HealthInputs {
            obligation: ObligationView {
                pending: true,
                active: false,
                unsettled_since: Some(MonoRawNs(10)),
            },
            now_ns,
            alarm_ns: 20,
            writer_alive: true,
            clear_stuck: None,
        }
    }

    #[test]
    fn current_pending_uses_continuous_interval_and_exact_alarm_boundary() {
        assert_eq!(classify(inputs(29)).current, []);
        assert_eq!(
            classify(inputs(30)).current,
            [CurrentDegradation::PersistPending]
        );
        let mut active = inputs(30);
        active.obligation.pending = false;
        active.obligation.active = true;
        assert_eq!(
            classify(active).unsettled,
            Some(UnsettledView {
                since_ns: 10,
                overdue: true
            })
        );
        active.obligation = ObligationView {
            pending: false,
            active: false,
            unsettled_since: None,
        };
        assert_eq!(classify(active).current, []);
        assert_eq!(classify(active).unsettled, None);
    }

    #[test]
    fn writer_gone_is_independent_of_obligation_and_retirement() {
        let mut value = inputs(30);
        value.writer_alive = false;
        assert_eq!(
            classify(value).current,
            [
                CurrentDegradation::PersistPending,
                CurrentDegradation::WriterGone
            ]
        );
        value.obligation = ObligationView {
            pending: false,
            active: false,
            unsettled_since: None,
        };
        assert_eq!(classify(value).current, [CurrentDegradation::WriterGone]);
        value.clear_stuck = Some(StuckReason::WriterGone);
        value.writer_alive = true;
        assert_eq!(classify(value).current, []);
    }

    #[test]
    fn clear_stuck_reasons_do_not_hide_other_current_degradations() {
        for (reason, expected) in [
            (
                StuckReason::RepairExhausted,
                CurrentDegradation::RepairExhausted,
            ),
            (StuckReason::MarkerLost, CurrentDegradation::MarkerLost),
            (
                StuckReason::ProtocolBug(ProtocolFault::IdentityMismatch),
                CurrentDegradation::ProtocolBug,
            ),
            (StuckReason::PairMismatch, CurrentDegradation::PairMismatch),
            (
                StuckReason::RetireRefused(Refusal::CorruptedPhase),
                CurrentDegradation::RetireRefused,
            ),
        ] {
            let mut value = inputs(30);
            value.writer_alive = false;
            value.clear_stuck = Some(reason);
            assert_eq!(
                classify(value).current,
                [
                    CurrentDegradation::PersistPending,
                    CurrentDegradation::WriterGone,
                    expected
                ]
            );
            value.clear_stuck = None;
            assert_eq!(
                classify(value).current,
                [
                    CurrentDegradation::PersistPending,
                    CurrentDegradation::WriterGone
                ]
            );
        }
    }

    #[test]
    fn sticky_edge_occurs_once_and_never_drives_current_truth() {
        let mut tracker = OverdueTracker::new();
        assert_eq!(tracker.observe(&classify(inputs(29))), None);
        let first = StickyEdge { since_ns: 10 };
        assert_eq!(tracker.observe(&classify(inputs(30))), Some(first));
        assert_eq!(tracker.observe(&classify(inputs(31))), None);
        let mut value = inputs(100);
        value.obligation = ObligationView {
            pending: false,
            active: false,
            unsettled_since: None,
        };
        let settled = classify(value);
        assert_eq!(settled.current, []);
        assert_eq!(tracker.observe(&settled), None);
        assert_eq!(tracker.first(), Some(first));
        value.obligation = ObligationView {
            pending: true,
            active: false,
            unsettled_since: Some(MonoRawNs(40)),
        };
        assert_eq!(tracker.observe(&classify(value)), None);
        assert_eq!(tracker.first(), Some(first));
    }
}
