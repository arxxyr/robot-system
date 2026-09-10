//! 自 main 首语句启动的独立监督器，只观测原子与单调系统时钟。
//!
//! 执法路径直接 `_exit(88)`，不依赖日志、锁、业务 worker 或析构完成。

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use robot_core::adapter::MAX_JOINTS;
use robot_core::control_authority::ControlAuthority;
use robot_core::time::{DurationNs, MonoRawNs, RobotClock, SystemClock};

use crate::latch_writer::{
    Budgets, Candidate, CandidateKind, Decision, Inputs, PersistShared, ProgressDecodeError,
    RuntimeWords, ShutdownTicket, arm, confirm, evaluate,
};

pub const PRE_BOOT_BUDGET: DurationNs = DurationNs(5_000_000_000);
pub const STARTUP_ACK_BUDGET: DurationNs = DurationNs(2_000_000_000);
pub const SHUTDOWN_MARGIN: DurationNs = DurationNs(1_000_000_000);
pub const RUNTIME_SHUTDOWN_BUDGET: DurationNs = DurationNs(10_000_000_000);

/// 一次安装的原权限与 writer 引用；不复制任何权限字或持久化状态。
pub struct RuntimeBinding {
    authority: Arc<ControlAuthority>,
    writer: Arc<PersistShared>,
}

impl RuntimeBinding {
    #[must_use]
    pub const fn authority(&self) -> &Arc<ControlAuthority> {
        &self.authority
    }
    #[must_use]
    pub const fn writer(&self) -> &Arc<PersistShared> {
        &self.writer
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeInstallError {
    AlreadyInstalled,
    BaseMismatch {
        expected: MonoRawNs,
        obligation: MonoRawNs,
        progress: MonoRawNs,
        announce: MonoRawNs,
    },
}

pub struct RuntimeInstallRefused {
    pub authority: Arc<ControlAuthority>,
    pub writer: Arc<PersistShared>,
    pub reason: RuntimeInstallError,
}

/// 只有真实安装成功才能取得此值；发布前真实 READY 发送由 main 完成。
#[must_use]
pub struct ReadyPermit<'a> {
    shared: &'a BootShared,
    binding: RuntimeBinding,
}

impl ReadyPermit<'_> {
    pub fn publish_ready(self, ready_at: MonoRawNs) {
        #[cfg(feature = "test-comm-fault-hook")]
        if let Some(page) = self.shared.observation() {
            use crate::comm_fault_observation::ObservationField as F;
            page.store(F::Ready, ready_at.0);
            page.store(F::ReadyReached, 1);
        }
        #[cfg(not(feature = "test-comm-fault-hook"))]
        let _ = ready_at;
        // 唯一非 Clone permit 是唯一发布者；重复 set 不替换已发布原件。
        let _ = self.shared.ready_binding.set(self.binding);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeSampleError {
    MissingShared,
    Progress(ProgressDecodeError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeFaultWordDecodeError {
    pub code: u8,
}

#[must_use]
pub const fn boot_budget(joints: usize) -> DurationNs {
    DurationNs(
        (joints as u64)
            .saturating_mul(200_000_000)
            .saturating_add(2_500_000_000),
    )
}

#[must_use]
pub const fn absolute_boot_budget(joints: usize) -> DurationNs {
    DurationNs(
        PRE_BOOT_BUDGET
            .0
            .saturating_add(boot_budget(joints).0)
            .saturating_add(STARTUP_ACK_BUDGET.0),
    )
}

pub struct BootShared {
    pub spawned_reached: AtomicBool,
    pub joint_count: AtomicUsize,
    pub supervisor_fired: AtomicU8,
    pub spawned_at_ns: AtomicU64,
    started_at: MonoRawNs,
    binding: OnceLock<RuntimeBinding>,
    ready_binding: OnceLock<RuntimeBinding>,
    runtime: RuntimeWords,
    first_sample_fault: AtomicU8,
    clear_retired_epoch: AtomicU64,
    #[cfg(feature = "test-comm-fault-hook")]
    observation: std::sync::OnceLock<Arc<crate::comm_fault_observation::ObservationPage>>,
}

impl BootShared {
    #[must_use]
    pub fn new(started_at: MonoRawNs) -> Self {
        Self {
            spawned_reached: AtomicBool::new(false),
            joint_count: AtomicUsize::new(MAX_JOINTS),
            supervisor_fired: AtomicU8::new(0),
            spawned_at_ns: AtomicU64::new(0),
            started_at,
            binding: OnceLock::new(),
            ready_binding: OnceLock::new(),
            runtime: RuntimeWords::new(),
            first_sample_fault: AtomicU8::new(0),
            clear_retired_epoch: AtomicU64::new(0),
            #[cfg(feature = "test-comm-fault-hook")]
            observation: std::sync::OnceLock::new(),
        }
    }
}

impl BootShared {
    #[must_use]
    pub const fn started_at(&self) -> MonoRawNs {
        self.started_at
    }
    #[must_use]
    pub fn binding(&self) -> Option<&RuntimeBinding> {
        self.binding.get()
    }
    #[must_use]
    pub fn ready_reached(&self) -> bool {
        self.ready_binding.get().is_some()
    }
    fn ready_runtime(&self) -> Option<&RuntimeBinding> {
        self.ready_binding.get()
    }
    #[must_use]
    pub const fn runtime_words(&self) -> &RuntimeWords {
        &self.runtime
    }
    #[must_use]
    pub fn clear_retired_for(&self, g: u64) -> bool {
        g != 0 && self.clear_retired_epoch.load(Ordering::Acquire) == g
    }

    fn install_runtime(
        &self,
        authority: Arc<ControlAuthority>,
        writer: Arc<PersistShared>,
    ) -> Result<ReadyPermit<'_>, RuntimeInstallRefused> {
        let reason = match self.binding.get() {
            Some(_) => Some(RuntimeInstallError::AlreadyInstalled),
            None if writer.obligation.base() != self.started_at
                || writer.progress.base() != self.started_at
                || writer.announce.base() != self.started_at =>
            {
                Some(RuntimeInstallError::BaseMismatch {
                    expected: self.started_at,
                    obligation: writer.obligation.base(),
                    progress: writer.progress.base(),
                    announce: writer.announce.base(),
                })
            }
            None => None,
        };
        if let Some(reason) = reason {
            return Err(RuntimeInstallRefused {
                authority,
                writer,
                reason,
            });
        }
        let ready_binding = RuntimeBinding {
            authority: Arc::clone(&authority),
            writer: Arc::clone(&writer),
        };
        match self.binding.set(RuntimeBinding { authority, writer }) {
            Ok(()) => Ok(ReadyPermit {
                shared: self,
                binding: ready_binding,
            }),
            Err(binding) => Err(RuntimeInstallRefused {
                authority: binding.authority,
                writer: binding.writer,
                reason: RuntimeInstallError::AlreadyInstalled,
            }),
        }
    }

    /// 测试复用真实整对安装；不启动监督、不自动发布 READY 或任何关闭事实。
    #[cfg(any(test, feature = "test-comm-fault-hook"))]
    pub fn install_runtime_for_test(
        &self,
        authority: Arc<ControlAuthority>,
        writer: Arc<PersistShared>,
    ) -> Result<ReadyPermit<'_>, RuntimeInstallRefused> {
        self.install_runtime(authority, writer)
    }

    pub(crate) fn publish_clear_retired<'a>(
        &self,
        proof: crate::ipc_thread::ClearClosureProof<'a>,
    ) -> Result<(), crate::ipc_thread::ClearClosurePublishRefused<'a>> {
        use crate::ipc_thread::{ClearClosurePublishRefused, ClosureMismatch};
        let g = match proof.checked_epoch_for(self) {
            Ok(g) => g.get(),
            Err(reason) => return Err(ClearClosurePublishRefused { proof, reason }),
        };
        match self
            .clear_retired_epoch
            .compare_exchange(0, g, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => Ok(()),
            Err(previous) if previous == g => Ok(()),
            Err(_) => Err(ClearClosurePublishRefused {
                proof,
                reason: ClosureMismatch::PublishedGeneration,
            }),
        }
    }

    /// 主线程、IPC 与监督器争用同一关闭字；只有原 arm 赢家推进权限代次。
    pub fn arm_runtime_shutdown(&self, now: MonoRawNs) -> Option<ShutdownTicket> {
        self.binding().map(|binding| {
            arm(&self.runtime, now, RUNTIME_SHUTDOWN_BUDGET, || {
                binding.authority.shutdown().epoch
            })
        })
    }

    pub fn first_runtime_sample_fault(
        &self,
    ) -> Result<Option<RuntimeSampleError>, RuntimeFaultWordDecodeError> {
        match self.first_sample_fault.load(Ordering::Acquire) {
            0 => Ok(None),
            1 => Ok(Some(RuntimeSampleError::MissingShared)),
            2 => Ok(Some(RuntimeSampleError::Progress(ProgressDecodeError(6)))),
            3 => Ok(Some(RuntimeSampleError::Progress(ProgressDecodeError(7)))),
            code => Err(RuntimeFaultWordDecodeError { code }),
        }
    }

    fn record_first_sample_error(&self, error: RuntimeSampleError) {
        let code = match error {
            RuntimeSampleError::MissingShared => 1,
            RuntimeSampleError::Progress(ProgressDecodeError(6)) => 2,
            RuntimeSampleError::Progress(_) => 3,
        };
        let _ =
            self.first_sample_fault
                .compare_exchange(0, code, Ordering::Release, Ordering::Relaxed);
    }

    #[cfg(feature = "test-comm-fault-hook")]
    #[must_use]
    pub fn observation(&self) -> Option<&crate::comm_fault_observation::ObservationPage> {
        self.observation.get().map(Arc::as_ref)
    }

    #[cfg(feature = "test-comm-fault-hook")]
    #[must_use]
    pub fn observation_arc(&self) -> Option<Arc<crate::comm_fault_observation::ObservationPage>> {
        self.observation.get().map(Arc::clone)
    }
}

pub struct BootSupervisor {
    shared: Arc<BootShared>,
    started_at: MonoRawNs,
    // handle 丢弃只 detach；启动失败/帮助路径绝不能等待监督器退出。
    _join: JoinHandle<()>,
}

impl BootSupervisor {
    /// 必须在日志与 CLI 解析之前调用。
    ///
    /// # Errors
    ///
    /// 监督线程本身无法启动时，调用者立即退 1。
    pub fn start() -> io::Result<Self> {
        let clock = SystemClock::new();
        let started_at = clock.now();
        let shared = Arc::new(BootShared::new(started_at));
        let thread_shared = Arc::clone(&shared);
        let join = thread::Builder::new()
            .name("s0_boot_supervisor".into())
            .spawn(move || supervise(&thread_shared, started_at, clock))?;
        let supervisor = Self {
            shared,
            started_at,
            _join: join,
        };
        #[cfg(feature = "test-comm-fault-hook")]
        supervisor.install_observation()?;
        #[cfg(feature = "test-comm-fault-hook")]
        crate::comm_fault_trace::install_from_env()?;
        Ok(supervisor)
    }

    #[must_use]
    pub fn shared(&self) -> Arc<BootShared> {
        Arc::clone(&self.shared)
    }

    #[must_use]
    pub const fn started_at(&self) -> MonoRawNs {
        self.started_at
    }

    /// 配置解析只允许缩短已承诺的绝对启动预算。
    pub fn update_joint_count(&self, count: usize) {
        self.shared
            .joint_count
            .fetch_min(count.min(MAX_JOINTS), Ordering::Release);
        #[cfg(feature = "test-comm-fault-hook")]
        if let Some(page) = self.shared.observation() {
            page.store(
                crate::comm_fault_observation::ObservationField::JointCount,
                self.shared.joint_count.load(Ordering::Acquire) as u64,
            );
        }
    }

    /// 同一个时间戳供 serial 首 IO 门与主线程等待截止使用。
    pub fn mark_workers_spawned(&self, now: MonoRawNs) {
        self.shared.spawned_at_ns.store(now.0, Ordering::Relaxed);
        self.shared.spawned_reached.store(true, Ordering::Release);
        #[cfg(feature = "test-comm-fault-hook")]
        if let Some(page) = self.shared.observation() {
            page.store_first(
                crate::comm_fault_observation::ObservationField::Spawned,
                now.0,
            );
        }
    }

    /// 同基准的整对安装；拒绝按值返回输入，不创建另一权限或 writer。
    pub fn install_runtime(
        &self,
        authority: Arc<ControlAuthority>,
        writer: Arc<PersistShared>,
    ) -> Result<ReadyPermit<'_>, RuntimeInstallRefused> {
        self.shared.install_runtime(authority, writer)
    }

    #[cfg(feature = "test-comm-fault-hook")]
    fn install_observation(&self) -> io::Result<()> {
        use crate::comm_fault_observation::{ObservationField as F, ObservationPage, preinit_ns};

        let Some(path) = std::env::var_os("ROBOT_TEST_OBSERVATION_PATH") else {
            return Ok(());
        };
        // 此时监督线程已经真正运行；打开或 mmap 停顿仍由 CP1 执法。
        let page = Arc::new(ObservationPage::open(std::path::Path::new(&path))?);
        page.store(F::MainStart, self.started_at.0);
        page.store(F::Preinit, preinit_ns());
        page.store(F::JointCount, MAX_JOINTS as u64);
        self.shared
            .observation
            .set(page)
            .map_err(|_| io::Error::other("观测页不得重复安装"))?;
        if let Some(page) = self.shared.observation() {
            page.store(F::ObservationReady, 1);
        }
        Ok(())
    }
}

/// 原共享字的实际采样；不从错误恢复为虚构的 Idle。
pub fn sample_runtime(
    shared: &BootShared,
    armed_at: MonoRawNs,
    now: MonoRawNs,
    budgets: Budgets,
) -> Result<Inputs, RuntimeSampleError> {
    sample_binding(shared.binding(), &shared.runtime, armed_at, now, budgets)
}

fn sample_binding(
    binding: Option<&RuntimeBinding>,
    words: &RuntimeWords,
    armed_at: MonoRawNs,
    now: MonoRawNs,
    budgets: Budgets,
) -> Result<Inputs, RuntimeSampleError> {
    let binding = binding.ok_or(RuntimeSampleError::MissingShared)?;
    Ok(Inputs {
        progress: binding
            .writer
            .progress
            .load()
            .map_err(RuntimeSampleError::Progress)?,
        announce: binding.writer.announce.offered_at(),
        obligation: binding.writer.obligation.view(),
        armed_at,
        shutdown_deadline: words.shutdown_deadline(),
        now,
        budgets,
    })
}

fn runtime_decision(
    shared: &BootShared,
    binding: &RuntimeBinding,
    armed_at: MonoRawNs,
    now: MonoRawNs,
) -> Decision {
    match sample_runtime(shared, armed_at, now, Budgets::default()) {
        Ok(inputs) => {
            #[cfg(feature = "test-comm-fault-hook")]
            if inputs.progress.phase == crate::latch_writer::Phase::Exited
                && let Some(page) = shared.observation()
            {
                page.store_first(
                    crate::comm_fault_observation::ObservationField::WriterExitedAt,
                    inputs.progress.entered_at.0,
                );
            }
            evaluate(&inputs)
        }
        Err(error) => {
            shared.record_first_sample_error(error);
            let ticket = arm(&shared.runtime, now, RUNTIME_SHUTDOWN_BUDGET, || {
                binding.authority.shutdown().epoch
            });
            let candidate = Candidate {
                kind: CandidateKind::Shutdown,
                deadline: ticket.deadline(),
            };
            Decision {
                candidates: [None, None, None, Some(candidate)],
                fire: (now.0 >= candidate.deadline.0).then_some(candidate),
            }
        }
    }
}

fn supervise(shared: &BootShared, started_at: MonoRawNs, clock: SystemClock) {
    let mut armed_at = None;
    loop {
        if let Some(binding) = shared.ready_runtime() {
            let start = *armed_at.get_or_insert_with(|| {
                let now = clock.now();
                #[cfg(feature = "test-comm-fault-hook")]
                if let Some(page) = shared.observation() {
                    page.store_first(
                        crate::comm_fault_observation::ObservationField::RuntimeArmed,
                        now.0,
                    );
                }
                now
            });
            let before = runtime_decision(shared, binding, start, clock.now());
            if before.fire.is_some() {
                let after = runtime_decision(shared, binding, start, clock.now());
                if let Some(candidate) = confirm(&before, &after) {
                    #[cfg(feature = "test-comm-fault-hook")]
                    if let Some(page) = shared.observation() {
                        use crate::comm_fault_observation::ObservationField as F;
                        let reason = match candidate.kind {
                            CandidateKind::FileStep => 1,
                            CandidateKind::Pickup => 2,
                            CandidateKind::Shutdown => 3,
                            CandidateKind::WriterExited => 4,
                        };
                        let checkpoint = match candidate.kind {
                            CandidateKind::Shutdown => 4,
                            CandidateKind::FileStep
                            | CandidateKind::Pickup
                            | CandidateKind::WriterExited => 3,
                        };
                        page.store(F::Fired, clock.now().0);
                        page.store(F::FiredReason, reason);
                        page.store(F::FiredCheckpoint, checkpoint);
                    }
                    #[cfg(not(feature = "test-comm-fault-hook"))]
                    let _ = candidate;
                    // SAFETY: 既定监督出口终止本进程，不执行可能阻塞的析构或日志。
                    unsafe { libc::_exit(90) };
                }
            }
            thread::sleep(Duration::from_millis(10));
            continue;
        }
        let now = clock.now();
        let spawned = shared.spawned_reached.load(Ordering::Acquire);
        let joints = shared.joint_count.load(Ordering::Acquire);
        let checkpoint = match () {
            () if !spawned && now.0 >= started_at.0.saturating_add(PRE_BOOT_BUDGET.0) => 1,
            () if now.0
                >= started_at
                    .0
                    .saturating_add(absolute_boot_budget(joints).0)
                    .saturating_add(SHUTDOWN_MARGIN.0) =>
            {
                2
            }
            () => 0,
        };
        if checkpoint != 0 {
            shared.supervisor_fired.store(checkpoint, Ordering::Relaxed);
            #[cfg(feature = "test-comm-fault-hook")]
            if let Some(page) = shared.observation() {
                use crate::comm_fault_observation::ObservationField as F;
                page.store(F::Fired, now.0);
                page.store(F::FiredCheckpoint, u64::from(checkpoint));
            }
            // SAFETY: 原 CP1/CP2 只终止当前进程，故意绕过可能阻塞的析构。
            unsafe { libc::_exit(88) };
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_budget_census_matches_six_thirteen_and_thirty_two_joints() {
        for (joints, boot_ns, absolute_ns) in [
            (6, 3_700_000_000, 10_700_000_000),
            (13, 5_100_000_000, 12_100_000_000),
            (32, 8_900_000_000, 15_900_000_000),
        ] {
            assert_eq!(boot_budget(joints).0, boot_ns);
            assert_eq!(absolute_boot_budget(joints).0, absolute_ns);
        }
    }

    fn test_authority() -> Arc<ControlAuthority> {
        Arc::new(
            ControlAuthority::new(robot_core::control_authority::AuthorityInit::new(
                false,
                robot_core::executor_ctrl::OutputSafetyState::TorqueOffConfirmed,
                robot_core::control_authority::InhibitSet::EMPTY,
            ))
            .unwrap(),
        )
    }

    #[test]
    fn runtime_install_returns_original_pair_and_keeps_one_ready_and_shutdown_generation() {
        let base = MonoRawNs(100);
        let shared = BootShared::new(base);
        let authority = test_authority();
        let bad = Arc::new(PersistShared::new(MonoRawNs(101)).unwrap());
        let Err(refused) = shared.install_runtime(authority.clone(), bad.clone()) else {
            panic!("错base不能安装")
        };
        assert!(
            matches!(refused.reason,RuntimeInstallError::BaseMismatch{expected,..} if expected==base)
        );
        assert!(Arc::ptr_eq(&refused.authority, &authority) && Arc::ptr_eq(&refused.writer, &bad));
        assert!(shared.binding().is_none() && !shared.ready_reached());
        let writer = Arc::new(PersistShared::new(base).unwrap());
        let permit = shared
            .install_runtime(authority.clone(), writer.clone())
            .unwrap_or_else(|_| panic!("原pair安装"));
        let binding = shared.binding().unwrap();
        assert!(
            Arc::ptr_eq(binding.authority(), &authority) && Arc::ptr_eq(binding.writer(), &writer)
        );
        assert!(!shared.ready_reached());
        let other = test_authority();
        let other_writer = Arc::new(PersistShared::new(base).unwrap());
        let Err(refused) = shared.install_runtime(other.clone(), other_writer.clone()) else {
            panic!("重复安装")
        };
        assert_eq!(refused.reason, RuntimeInstallError::AlreadyInstalled);
        assert!(
            Arc::ptr_eq(&refused.authority, &other) && Arc::ptr_eq(&refused.writer, &other_writer)
        );
        let original = shared.arm_runtime_shutdown(MonoRawNs(200)).unwrap();
        let epoch = authority.snapshot().epoch;
        permit.publish_ready(MonoRawNs(300));
        assert!(shared.ready_reached());
        let again = shared.arm_runtime_shutdown(MonoRawNs(9_000)).unwrap();
        assert_eq!(again.deadline(), original.deadline());
        assert_eq!(shared.runtime_words().generation(), epoch.get());
        assert_eq!(authority.snapshot().epoch, epoch);
        assert!(!shared.clear_retired_for(0) && !shared.clear_retired_for(epoch.get()));
    }

    #[test]
    fn runtime_sample_rejection_keeps_first_cause_and_original_deadline_after_recovery() {
        // 缺绑定在正常READY后不可自然产生；调用同一私有决策边界，不伪称真实坏mailbox。
        let base = MonoRawNs(100);
        let shared = BootShared::new(base);
        let authority = test_authority();
        let writer = Arc::new(PersistShared::new(base).unwrap());
        let binding = RuntimeBinding {
            authority: authority.clone(),
            writer: writer.clone(),
        };
        assert!(matches!(
            sample_binding(
                None,
                &shared.runtime,
                base,
                MonoRawNs(200),
                Budgets::default()
            ),
            Err(RuntimeSampleError::MissingShared)
        ));
        let first = runtime_decision(&shared, &binding, base, MonoRawNs(200));
        let deadline = shared.runtime_words().shutdown_deadline().unwrap();
        let epoch = authority.snapshot().epoch;
        assert!(first.fire.is_none());
        shared.record_first_sample_error(RuntimeSampleError::Progress(ProgressDecodeError(7)));
        assert_eq!(
            shared.first_runtime_sample_fault(),
            Ok(Some(RuntimeSampleError::MissingShared))
        );
        let permit = shared
            .install_runtime(authority.clone(), writer)
            .unwrap_or_else(|_| panic!("实际恢复绑定"));
        permit.publish_ready(MonoRawNs(300));
        let restored = runtime_decision(&shared, shared.binding().unwrap(), base, deadline);
        assert!(
            matches!(restored.fire,Some(candidate) if candidate.kind==CandidateKind::Shutdown&&candidate.deadline==deadline)
        );
        assert_eq!(shared.runtime_words().shutdown_deadline(), Some(deadline));
        assert_eq!(authority.snapshot().epoch, epoch);
        assert_eq!(
            shared.first_runtime_sample_fault(),
            Ok(Some(RuntimeSampleError::MissingShared))
        );
    }
}
