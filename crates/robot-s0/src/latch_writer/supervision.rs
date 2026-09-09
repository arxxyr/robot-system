//! 运行期监督的纯机制：一次武装的关闭字、四类候选截止的比较与二次确认。
//!
//! 不写观测页、不退出进程、不宣称真实 daemon 关闭；生产调用者仅 W3。
//!
//! - [`arm`]：`shutdown_deadline_ns` CAS 0 → D 只武装一次；赢家才调用 `take_epoch()`
//!   取真实 epoch 并发布到 `shutdown_request_epoch`，输家只读；`generation() == 0`
//!   表示赢家尚未发布。
//! - [`evaluate`]：显式比较 `FileStep` / `Pickup` / `WriterExited` / `Shutdown` 四候选，
//!   `fire` 为已到期候选中最早者，同刻按 `FileStep` → `Pickup` → `WriterExited` → `Shutdown`；
//!   只有 `WriterExited` 受 D 武装特例（武装后由 `Shutdown` 接管），其余继续竞争。
//! - [`confirm`]：读 → 算 → 重读，两次 `fire` 不等则放弃本次触发。

use std::sync::atomic::{AtomicU64, Ordering};

use robot_core::control_authority::PermissionEpoch;
use robot_core::time::{DurationNs, MonoRawNs};

use super::obligation::ObligationView;
use super::progress::{Phase, ProgressView};

/// 运行期共享字（W3 嵌入 `BootShared`；本包独立结构）。
#[derive(Debug, Default)]
pub struct RuntimeWords {
    /// 关闭绝对截止 D；0 = 未武装。
    pub shutdown_deadline_ns: AtomicU64,
    /// 赢家发布的真实关闭代次 g；0 = 尚未发布。
    pub shutdown_request_epoch: AtomicU64,
    /// IPC 完成本代次 Lower 后写入 g。
    pub lowering_ack_epoch: AtomicU64,
}

impl RuntimeWords {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            shutdown_deadline_ns: AtomicU64::new(0),
            shutdown_request_epoch: AtomicU64::new(0),
            lowering_ack_epoch: AtomicU64::new(0),
        }
    }

    /// 已武装的 D；未武装为 `None`。
    #[must_use]
    pub fn shutdown_deadline(&self) -> Option<MonoRawNs> {
        match self.shutdown_deadline_ns.load(Ordering::Acquire) {
            0 => None,
            deadline => Some(MonoRawNs(deadline)),
        }
    }

    /// 已发布的关闭代次；0 = 赢家尚未发布。
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.shutdown_request_epoch.load(Ordering::Acquire)
    }
}

/// `arm` 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShutdownTicket {
    deadline: MonoRawNs,
    winner: bool,
}

impl ShutdownTicket {
    #[must_use]
    pub const fn deadline(&self) -> MonoRawNs {
        self.deadline
    }

    /// 本次调用是否为唯一赢家（只有赢家取过 epoch）。
    #[must_use]
    pub const fn is_winner(&self) -> bool {
        self.winner
    }
}

/// 一次武装：CAS 0 → `now + budget`；赢家才取真实 epoch 并发布，输家取回已存在的 D。
pub fn arm(
    words: &RuntimeWords,
    now: MonoRawNs,
    budget: DurationNs,
    take_epoch: impl FnOnce() -> PermissionEpoch,
) -> ShutdownTicket {
    let proposed = now.0.saturating_add(budget.0).max(1);
    match words.shutdown_deadline_ns.compare_exchange(
        0,
        proposed,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => {
            let epoch = take_epoch();
            words
                .shutdown_request_epoch
                .store(epoch.get(), Ordering::Release);
            ShutdownTicket {
                deadline: MonoRawNs(proposed),
                winner: true,
            }
        }
        Err(existing) => ShutdownTicket {
            deadline: MonoRawNs(existing),
            winner: false,
        },
    }
}

/// 候选种类；数值即同刻优先序。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CandidateKind {
    FileStep = 0,
    Pickup = 1,
    WriterExited = 2,
    Shutdown = 3,
}

/// 一个候选截止。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Candidate {
    pub kind: CandidateKind,
    pub deadline: MonoRawNs,
}

/// 预算（软件政策：`B_step` 2 s、`B_pickup` 500 ms）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budgets {
    pub step: DurationNs,
    pub pickup: DurationNs,
}

impl Default for Budgets {
    fn default() -> Self {
        Self {
            step: DurationNs::from_secs(2),
            pickup: DurationNs::from_millis(500),
        }
    }
}

/// 一次评估的输入快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Inputs {
    pub progress: ProgressView,
    /// 在途命令公告的接纳时刻。
    pub announce: Option<MonoRawNs>,
    pub obligation: ObligationView,
    /// 运行期监督接管时刻（READY）；接管前的戳一律从此起算。
    pub armed_at: MonoRawNs,
    /// 已武装的关闭截止 D。
    pub shutdown_deadline: Option<MonoRawNs>,
    pub now: MonoRawNs,
    pub budgets: Budgets,
}

/// 评估结果：四候选（按 [`CandidateKind`] 顺序）与已到期的最早触发。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    pub candidates: [Option<Candidate>; 4],
    pub fire: Option<Candidate>,
}

fn later(a: MonoRawNs, b: MonoRawNs) -> MonoRawNs {
    MonoRawNs(a.0.max(b.0))
}

fn plus(at: MonoRawNs, budget: DurationNs) -> MonoRawNs {
    MonoRawNs(at.0.saturating_add(budget.0))
}

/// 显式比较四候选。
#[must_use]
pub fn evaluate(inputs: &Inputs) -> Decision {
    let progress = inputs.progress;
    let armed = inputs.armed_at;
    let budgets = inputs.budgets;

    let file_step = (progress.phase == Phase::FileStep).then(|| Candidate {
        kind: CandidateKind::FileStep,
        deadline: plus(later(progress.entered_at, armed), budgets.step),
    });

    let pickup_base = match progress.phase {
        Phase::FileStep | Phase::Exited => None,
        Phase::Claiming | Phase::Delivering | Phase::RetryBackoff => {
            Some(later(progress.entered_at, armed))
        }
        Phase::Idle => {
            let obligation_since = (!inputs.obligation.is_settled())
                .then_some(inputs.obligation.unsettled_since)
                .flatten();
            let waiting = match (inputs.announce, obligation_since) {
                (Some(a), Some(b)) => Some(MonoRawNs(a.0.min(b.0))),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            };
            waiting.map(|since| later(later(since, progress.entered_at), armed))
        }
    };
    let pickup = pickup_base.map(|base| Candidate {
        kind: CandidateKind::Pickup,
        deadline: plus(base, budgets.pickup),
    });

    let writer_exited = (progress.phase == Phase::Exited && inputs.shutdown_deadline.is_none())
        .then(|| Candidate {
            kind: CandidateKind::WriterExited,
            deadline: plus(later(progress.entered_at, armed), budgets.pickup),
        });

    let shutdown = inputs.shutdown_deadline.map(|deadline| Candidate {
        kind: CandidateKind::Shutdown,
        deadline,
    });

    let candidates = [file_step, pickup, writer_exited, shutdown];
    let fire = candidates
        .iter()
        .flatten()
        .filter(|candidate| candidate.deadline.0 <= inputs.now.0)
        .min_by_key(|candidate| (candidate.deadline.0, candidate.kind))
        .copied();
    Decision { candidates, fire }
}

/// 二次确认：重读后触发不等则放弃本次。
#[must_use]
pub fn confirm(before: &Decision, after: &Decision) -> Option<Candidate> {
    match (before.fire, after.fire) {
        (Some(first), Some(second)) if first == second => Some(first),
        _ => None,
    }
}

/// 义务区间告警：`!is_settled` 且 `now ≥ unsettled_since + alarm`；只对连续非 settled
/// 区间告警，相对新 Pending 允许保守提前，不作为退出依据。
#[must_use]
pub fn obligation_overdue(view: &ObligationView, now: MonoRawNs, alarm: DurationNs) -> bool {
    match (view.is_settled(), view.unsettled_since) {
        (false, Some(since)) => now.0 >= plus(since, alarm).0,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    const ARMED: MonoRawNs = MonoRawNs(10_000_000_000);

    fn view(phase: Phase, entered_at: MonoRawNs) -> ProgressView {
        ProgressView { phase, entered_at }
    }

    fn settled() -> ObligationView {
        ObligationView {
            pending: false,
            active: false,
            unsettled_since: None,
        }
    }

    fn inputs(progress: ProgressView, now: MonoRawNs) -> Inputs {
        Inputs {
            progress,
            announce: None,
            obligation: settled(),
            armed_at: ARMED,
            shutdown_deadline: None,
            now,
            budgets: Budgets::default(),
        }
    }

    fn kinds(decision: &Decision) -> Vec<CandidateKind> {
        decision
            .candidates
            .iter()
            .flatten()
            .map(|c| c.kind)
            .collect()
    }

    #[test]
    fn file_step_deadline_uses_max_of_entry_and_armed() {
        // 跨 READY：进入早于 armed，自 armed 起算。
        let early = inputs(view(Phase::FileStep, MonoRawNs(1)), ARMED);
        let decision = evaluate(&early);
        assert_eq!(kinds(&decision), vec![CandidateKind::FileStep]);
        assert_eq!(
            decision.candidates[0].unwrap().deadline,
            plus(ARMED, DurationNs::from_secs(2))
        );
        assert_eq!(decision.fire, None);
        // 到期即触发。
        let due = inputs(
            view(Phase::FileStep, MonoRawNs(ARMED.0 + 1)),
            plus(MonoRawNs(ARMED.0 + 1), DurationNs::from_secs(2)),
        );
        assert_eq!(evaluate(&due).fire.unwrap().kind, CandidateKind::FileStep);
    }

    #[test]
    fn pickup_candidates_cover_claiming_delivering_backoff_and_idle_waiting() {
        for phase in [Phase::Claiming, Phase::Delivering, Phase::RetryBackoff] {
            let decision = evaluate(&inputs(view(phase, MonoRawNs(ARMED.0 + 100)), ARMED));
            assert_eq!(kinds(&decision), vec![CandidateKind::Pickup], "{phase:?}");
            assert_eq!(
                decision.candidates[1].unwrap().deadline,
                plus(MonoRawNs(ARMED.0 + 100), DurationNs::from_millis(500))
            );
        }
        // Idle 且无等待：无候选（空闲暂停不触发）。
        assert!(
            kinds(&evaluate(&inputs(view(Phase::Idle, ARMED), ARMED))).is_empty(),
            "空闲暂停不触发"
        );
        // Idle + 已接纳命令：max(accepted, idle_entered, armed) + pickup。
        let mut waiting = inputs(view(Phase::Idle, MonoRawNs(ARMED.0 + 50)), ARMED);
        waiting.announce = Some(MonoRawNs(ARMED.0 + 300));
        let decision = evaluate(&waiting);
        assert_eq!(
            decision.candidates[1].unwrap().deadline,
            plus(MonoRawNs(ARMED.0 + 300), DurationNs::from_millis(500))
        );
        // Idle + 义务未清偿（起点早于 idle 进入）：以 idle 进入时刻起算。
        let mut obligation = inputs(view(Phase::Idle, MonoRawNs(ARMED.0 + 900)), ARMED);
        obligation.obligation = ObligationView {
            pending: true,
            active: false,
            unsettled_since: Some(MonoRawNs(ARMED.0 + 100)),
        };
        assert_eq!(
            evaluate(&obligation).candidates[1].unwrap().deadline,
            plus(MonoRawNs(ARMED.0 + 900), DurationNs::from_millis(500))
        );
        // 两者都在：取较早的等待起点。
        obligation.announce = Some(MonoRawNs(ARMED.0 + 50));
        assert_eq!(
            evaluate(&obligation).candidates[1].unwrap().deadline,
            plus(MonoRawNs(ARMED.0 + 900), DurationNs::from_millis(500))
        );
    }

    #[test]
    fn writer_exited_is_a_candidate_only_before_arming_and_shutdown_takes_over() {
        // D 前：Exited 候选 = max(exited, armed) + pickup，含 READY 前退出。
        let before = inputs(view(Phase::Exited, MonoRawNs(5)), ARMED);
        let decision = evaluate(&before);
        assert_eq!(kinds(&decision), vec![CandidateKind::WriterExited]);
        assert_eq!(
            decision.candidates[2].unwrap().deadline,
            plus(ARMED, DurationNs::from_millis(500))
        );
        // D 后：撤销 Exited，由 Shutdown 候选接管。
        let mut after = before;
        after.shutdown_deadline = Some(MonoRawNs(ARMED.0 + 10_000_000_000));
        let decision = evaluate(&after);
        assert_eq!(kinds(&decision), vec![CandidateKind::Shutdown]);
        // FileStep 在 D 后仍有效并可早于 D 触发。
        let mut step = inputs(
            view(Phase::FileStep, ARMED),
            plus(ARMED, DurationNs::from_secs(2)),
        );
        step.shutdown_deadline = Some(MonoRawNs(ARMED.0 + 10_000_000_000));
        let decision = evaluate(&step);
        assert_eq!(
            kinds(&decision),
            vec![CandidateKind::FileStep, CandidateKind::Shutdown]
        );
        assert_eq!(decision.fire.unwrap().kind, CandidateKind::FileStep);
        // D 早于 FileStep：Shutdown 先触发。
        let mut d_first = step;
        d_first.shutdown_deadline = Some(MonoRawNs(ARMED.0 + 1_000_000_000));
        d_first.now = MonoRawNs(ARMED.0 + 1_000_000_000);
        assert_eq!(
            evaluate(&d_first).fire.unwrap().kind,
            CandidateKind::Shutdown
        );
    }

    #[test]
    fn tie_breaks_file_step_then_pickup_then_shutdown() {
        let deadline = plus(ARMED, DurationNs::from_secs(2));
        let mut tie = inputs(view(Phase::FileStep, ARMED), deadline);
        tie.shutdown_deadline = Some(deadline);
        assert_eq!(evaluate(&tie).fire.unwrap().kind, CandidateKind::FileStep);
        let mut pickup_tie = inputs(
            view(Phase::Claiming, ARMED),
            plus(ARMED, DurationNs::from_millis(500)),
        );
        pickup_tie.shutdown_deadline = Some(plus(ARMED, DurationNs::from_millis(500)));
        assert_eq!(
            evaluate(&pickup_tie).fire.unwrap().kind,
            CandidateKind::Pickup
        );
    }

    #[test]
    fn confirm_requires_identical_fire_on_reread() {
        let due = plus(ARMED, DurationNs::from_secs(2));
        let first = evaluate(&inputs(view(Phase::FileStep, ARMED), due));
        let same = evaluate(&inputs(view(Phase::FileStep, ARMED), due));
        assert_eq!(confirm(&first, &same), first.fire);
        // 重读时步骤已推进：放弃。
        let moved = evaluate(&inputs(view(Phase::FileStep, MonoRawNs(due.0 - 1)), due));
        assert_eq!(confirm(&first, &moved), None);
        let idle = evaluate(&inputs(view(Phase::Idle, due), due));
        assert_eq!(confirm(&first, &idle), None);
        assert_eq!(confirm(&idle, &idle), None, "无触发不确认");
    }

    #[test]
    fn arm_once_winner_takes_epoch_and_publishes_after_cas() {
        let words = RuntimeWords::new();
        assert_eq!(words.shutdown_deadline(), None);
        assert_eq!(words.generation(), 0);
        let taken = AtomicUsize::new(0);
        let ticket = arm(&words, MonoRawNs(1_000), DurationNs::from_secs(10), || {
            // 赢家发布前的空窗：CAS 已成功但 generation 仍为 0。
            assert_eq!(words.shutdown_deadline(), Some(MonoRawNs(10_000_001_000)));
            assert_eq!(words.generation(), 0);
            taken.fetch_add(1, Ordering::Relaxed);
            PermissionEpoch::new(42).unwrap()
        });
        assert!(ticket.is_winner());
        assert_eq!(ticket.deadline(), MonoRawNs(10_000_001_000));
        assert_eq!(words.generation(), 42);
        // 竞争者：不取 epoch、不改 D、不改 g。
        let loser = arm(&words, MonoRawNs(5_000), DurationNs::from_secs(1), || {
            taken.fetch_add(1, Ordering::Relaxed);
            PermissionEpoch::new(99).unwrap()
        });
        assert!(!loser.is_winner());
        assert_eq!(loser.deadline(), MonoRawNs(10_000_001_000));
        assert_eq!(words.generation(), 42);
        assert_eq!(taken.load(Ordering::Relaxed), 1, "只有赢家取 epoch");
        // Exhausted epoch（MAX）也是非 0 真实值，不伪造 g=0。
        let words = RuntimeWords::new();
        arm(&words, MonoRawNs(0), DurationNs::from_secs(1), || {
            PermissionEpoch::MAX
        });
        assert_eq!(words.generation(), PermissionEpoch::MAX.get());
        assert_ne!(words.generation(), 0);
    }

    #[test]
    fn obligation_overdue_only_for_unsettled_interval() {
        let alarm = DurationNs::from_secs(2);
        let pending = ObligationView {
            pending: true,
            active: false,
            unsettled_since: Some(MonoRawNs(1_000)),
        };
        assert!(!obligation_overdue(
            &pending,
            MonoRawNs(2_000_000_999),
            alarm
        ));
        assert!(obligation_overdue(
            &pending,
            MonoRawNs(2_000_001_000),
            alarm
        ));
        assert!(!obligation_overdue(&settled(), MonoRawNs(u64::MAX), alarm));
    }
}
