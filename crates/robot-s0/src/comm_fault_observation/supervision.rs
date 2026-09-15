//! R4c原实例的只读POD；一次快照先载荷后提交，健康重复帧另用版本门。

use super::{ObservationField as F, ObservationPage};
use crate::latch_writer::{ObligationView, PersistShared};
use std::sync::atomic::Ordering;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ObligationObservation {
    pub at_ns: u64,
    pub flags: u64,
    pub since_ns: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CurrentHealthObservation {
    pub version: u64,
    pub bits: u64,
    pub sampled_ns: u64,
    pub unsettled_since_ns: u64,
    pub overdue: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SupervisionSnapshot {
    pub mode: u64,
    pub command_txn: u128,
    pub command_reached_ns: u64,
    pub announce_ns: u64,
    pub idle_entry_ns: u64,
    pub obligation_id: u64,
    pub pending: ObligationObservation,
    pub active: ObligationObservation,
    pub step_stage: u64,
    pub step_entry_ns: u64,
    pub reached_ns: u64,
    pub main_hold_ns: u64,
    pub main_writer_observed_ns: u64,
    pub exited: ObligationObservation,
    pub fallback_tag: u64,
    pub fallback_settle: u64,
    pub fallback: ObligationObservation,
    pub pause_failure: u64,
    pub helper_owned: u64,
    pub helper_entered: u64,
    pub health_entered_ns: u64,
    pub systemd_entered_ns: u64,
    pub health: Option<CurrentHealthObservation>,
}

fn flags(view: ObligationView) -> u64 {
    u64::from(view.pending) | (u64::from(view.active) << 1)
}

impl ObservationPage {
    fn view_record(&self, view: ObligationView, fields: [F; 3], at_ns: u64) {
        // 各原拥有者只调用一次；不把重复写覆盖成另一个责任快照。
        if self.load(fields[2]) != 0 {
            return;
        }
        self.store(fields[0], flags(view));
        self.store(fields[1], view.unsettled_since.map_or(0, |at| at.0));
        self.store(fields[2], at_ns);
    }

    pub fn record_pending(&self, shared: &PersistShared, at_ns: u64) {
        self.store_first(F::ObligationId, shared.obligation.id().get());
        self.view_record(
            shared.obligation.view(),
            [F::PendingFlags, F::PendingSince, F::PendingAt],
            at_ns,
        );
    }

    pub fn record_active(&self, shared: &PersistShared, at_ns: u64) {
        self.view_record(
            shared.obligation.view(),
            [F::ActiveFlags, F::ActiveSince, F::ActiveAt],
            at_ns,
        );
    }

    pub fn record_writer_exited_view(&self, shared: &PersistShared, at_ns: u64) {
        self.view_record(
            shared.obligation.view(),
            [F::ExitedFlags, F::ExitedSince, F::ExitedObserved],
            at_ns,
        );
    }

    pub fn record_fallback(&self, tag: u64, settle: u64, shared: &PersistShared, at_ns: u64) {
        if self.load(F::FallbackObserved) != 0 {
            return;
        }
        self.store(F::FallbackTag, tag);
        self.store(F::FallbackSettle, settle);
        self.view_record(
            shared.obligation.view(),
            [F::FallbackFlags, F::FallbackSince, F::FallbackObserved],
            at_ns,
        );
    }

    /// 仅IPC原报告器有一个写者；诊断版本不是生产进度/义务。
    pub fn record_current_health(&self, snapshot: &robot_health::RuntimeHealthSnapshot) {
        use robot_health::RuntimeDegradation as D;
        let version = self.atomic(F::HealthVersion);
        let previous = version.load(Ordering::Relaxed);
        let Some(next) = previous.checked_add(2) else {
            return;
        };
        version.store(previous + 1, Ordering::Release);
        let bits = [
            D::PersistPending,
            D::WriterGone,
            D::RepairExhausted,
            D::MarkerLost,
            D::ProtocolBug,
            D::PairMismatch,
            D::RetireRefused,
        ]
        .into_iter()
        .enumerate()
        .fold(0, |bits, (index, kind)| {
            bits | (u64::from(snapshot.input.current.contains(kind)) << index)
        });
        self.store(F::CurrentHealthBits, bits);
        self.store(F::CurrentHealthAt, snapshot.sampled_ns);
        self.store(
            F::CurrentHealthSince,
            snapshot.input.unsettled.map_or(0, |v| v.since_ns),
        );
        self.store(
            F::CurrentHealthOverdue,
            u64::from(snapshot.input.unsettled.is_some_and(|v| v.overdue)),
        );
        version.store(next, Ordering::Release);
    }

    fn health_snapshot(&self) -> Option<CurrentHealthObservation> {
        // 写者被杀在奇数态也不阻塞父进程；本次取不到完整帧就明确None。
        for _ in 0..4 {
            let version = self.load(F::HealthVersion);
            if version == 0 || version & 1 != 0 {
                continue;
            }
            let result = CurrentHealthObservation {
                version,
                bits: self.load(F::CurrentHealthBits),
                sampled_ns: self.load(F::CurrentHealthAt),
                unsettled_since_ns: self.load(F::CurrentHealthSince),
                overdue: self.load(F::CurrentHealthOverdue) != 0,
            };
            if version == self.load(F::HealthVersion) {
                return Some(result);
            }
        }
        None
    }

    fn view_snapshot(&self, fields: [F; 3]) -> ObligationObservation {
        let at_ns = self.load(fields[2]);
        if at_ns == 0 {
            return ObligationObservation::default();
        }
        ObligationObservation {
            at_ns,
            flags: self.load(fields[0]),
            since_ns: self.load(fields[1]),
        }
    }

    pub(super) fn supervision_snapshot(&self) -> SupervisionSnapshot {
        let command_reached_ns = self.load(F::CommandReached);
        let reached_ns = self.load(F::SupReached);
        let helper_entered = self.load(F::HelperEntered);
        let fallback =
            self.view_snapshot([F::FallbackFlags, F::FallbackSince, F::FallbackObserved]);
        SupervisionSnapshot {
            mode: self.load(F::SupMode),
            command_txn: if command_reached_ns == 0 {
                0
            } else {
                u128::from(self.load(F::CommandTxnLow))
                    | (u128::from(self.load(F::CommandTxnHigh)) << 64)
            },
            command_reached_ns,
            announce_ns: self.load(F::AnnounceAt),
            idle_entry_ns: self.load(F::IdleEntry),
            obligation_id: self.load(F::ObligationId),
            pending: self.view_snapshot([F::PendingFlags, F::PendingSince, F::PendingAt]),
            active: self.view_snapshot([F::ActiveFlags, F::ActiveSince, F::ActiveAt]),
            step_stage: self.load(F::StepStage),
            step_entry_ns: self.load(F::StepEntry),
            reached_ns,
            main_hold_ns: self.load(F::MainHold),
            main_writer_observed_ns: self.load(F::MainWriterObserved),
            exited: self.view_snapshot([F::ExitedFlags, F::ExitedSince, F::ExitedObserved]),
            fallback_tag: if fallback.at_ns == 0 {
                0
            } else {
                self.load(F::FallbackTag)
            },
            fallback_settle: if fallback.at_ns == 0 {
                0
            } else {
                self.load(F::FallbackSettle)
            },
            fallback,
            pause_failure: self.load(F::TestPauseFailure),
            helper_owned: self.load(F::HelperOwned),
            helper_entered,
            health_entered_ns: self.load(F::HealthEnteredAt),
            systemd_entered_ns: self.load(F::SystemdEnteredAt),
            health: self.health_snapshot(),
        }
    }
}

impl ObservationPage {
    pub fn record_helper_owned(&self, kind: robot_health::test_comm_fault::HelperKind) {
        let bit = match kind {
            robot_health::test_comm_fault::HelperKind::HealthMonitor => 1,
            robot_health::test_comm_fault::HelperKind::SystemdWatchdog => 2,
        };
        self.merge_bits(F::HelperOwned, bit);
    }
    pub fn record_helper_entered(&self, value: robot_health::test_comm_fault::HelperEntered) {
        let (field, bit) = match value.kind {
            robot_health::test_comm_fault::HelperKind::HealthMonitor => (F::HealthEnteredAt, 1),
            robot_health::test_comm_fault::HelperKind::SystemdWatchdog => (F::SystemdEnteredAt, 2),
        };
        self.store_first(field, value.at_ns);
        self.merge_bits(F::HelperEntered, bit);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use robot_core::time::MonoRawNs;
    use robot_health::{
        RuntimeDegradation, RuntimeDegradations, RuntimeHealthInput, RuntimeHealthSnapshot,
        RuntimeUnsettled,
    };

    #[test]
    fn appended_supervision_words_preserve_layout_and_full_transaction_commit_gate() {
        assert_eq!(F::SupMode as usize, 39);
        assert_eq!(F::CurrentHealthOverdue as usize, 74);
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("page");
        let parent = ObservationPage::create(&path).unwrap();
        let child = ObservationPage::open(&path).unwrap();
        let txn = (0xfedc_ba98_7654_3210_u128 << 64) | 0x0123_4567_89ab_cdef;
        child.store(
            F::CommandTxnLow,
            u64::try_from(txn & u128::from(u64::MAX)).expect("已限定低64位"),
        );
        child.store(F::CommandTxnHigh, (txn >> 64) as u64);
        assert_eq!(parent.snapshot().supervision.command_txn, 0);
        child.store(F::CommandReached, 123);
        assert_eq!(parent.snapshot().supervision.command_txn, txn);
        assert_eq!(std::fs::metadata(path).unwrap().len(), 4096);
    }

    #[test]
    fn original_obligation_snapshots_publish_separate_pending_active_and_fallback_facts() {
        let directory = tempfile::tempdir().unwrap();
        let page = ObservationPage::create(&directory.path().join("page")).unwrap();
        let shared = PersistShared::new(MonoRawNs(10)).unwrap();
        shared.obligation.request(MonoRawNs(20));
        page.record_pending(&shared, 30);
        let claim = shared.obligation.claim_guard().unwrap();
        page.record_active(&shared, 40);
        drop(claim);
        page.record_writer_exited_view(&shared, 50);
        page.record_fallback(4, 2, &shared, 60);
        page.record_pending(&shared, 70);
        let observed = page.snapshot().supervision;
        assert_eq!(observed.obligation_id, shared.obligation.id().get());
        assert_eq!((observed.pending.flags, observed.pending.at_ns), (1, 30));
        assert_eq!((observed.active.flags, observed.active.at_ns), (2, 40));
        assert_eq!((observed.exited.flags, observed.exited.at_ns), (1, 50));
        assert_eq!(
            (
                observed.fallback_tag,
                observed.fallback_settle,
                observed.fallback.at_ns
            ),
            (4, 2, 60)
        );
        for view in [
            observed.pending,
            observed.active,
            observed.exited,
            observed.fallback,
        ] {
            assert_eq!(view.since_ns, 20);
        }
        assert!(shared.obligation.has_pending(), "诊断不清偿原义务");
    }

    #[test]
    fn current_health_repeated_frames_reject_incomplete_commit_and_clear_current_bits() {
        let directory = tempfile::tempdir().unwrap();
        let page = ObservationPage::create(&directory.path().join("page")).unwrap();
        assert!(page.snapshot().supervision.health.is_none());
        let mut current = RuntimeDegradations::empty();
        current.insert(RuntimeDegradation::PersistPending);
        page.record_current_health(&RuntimeHealthSnapshot {
            sampled_ns: 30,
            input: RuntimeHealthInput {
                current,
                unsettled: Some(RuntimeUnsettled {
                    since_ns: 20,
                    overdue: true,
                }),
            },
        });
        let first = page.snapshot().supervision.health.unwrap();
        assert_eq!(
            (
                first.version,
                first.bits,
                first.sampled_ns,
                first.unsettled_since_ns,
                first.overdue
            ),
            (2, 1, 30, 20, true)
        );
        page.store(F::HealthVersion, 3);
        page.store(F::CurrentHealthBits, 127);
        assert!(page.snapshot().supervision.health.is_none());
        page.store(F::HealthVersion, 2);
        page.record_current_health(&RuntimeHealthSnapshot {
            sampled_ns: 40,
            input: RuntimeHealthInput {
                current: RuntimeDegradations::empty(),
                unsettled: None,
            },
        });
        let second = page.snapshot().supervision.health.unwrap();
        assert_eq!(
            (
                second.version,
                second.bits,
                second.sampled_ns,
                second.unsettled_since_ns,
                second.overdue
            ),
            (4, 0, 40, 0, false)
        );
    }
}
