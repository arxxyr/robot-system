//! 自 main 首语句启动的独立监督器，只观测原子与单调系统时钟。
//!
//! 执法路径直接 `_exit(88)`，不依赖日志、锁、业务 worker 或析构完成。

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use robot_core::adapter::MAX_JOINTS;
use robot_core::time::{DurationNs, MonoRawNs, RobotClock, SystemClock};

pub const PRE_BOOT_BUDGET: DurationNs = DurationNs(5_000_000_000);
pub const STARTUP_ACK_BUDGET: DurationNs = DurationNs(2_000_000_000);
pub const SHUTDOWN_MARGIN: DurationNs = DurationNs(1_000_000_000);

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
    pub ready_reached: AtomicBool,
    pub joint_count: AtomicUsize,
    pub supervisor_fired: AtomicU8,
    pub spawned_at_ns: AtomicU64,
    #[cfg(feature = "test-comm-fault-hook")]
    observation: std::sync::OnceLock<Arc<crate::comm_fault_observation::ObservationPage>>,
}

impl Default for BootShared {
    fn default() -> Self {
        Self {
            spawned_reached: AtomicBool::new(false),
            ready_reached: AtomicBool::new(false),
            joint_count: AtomicUsize::new(MAX_JOINTS),
            supervisor_fired: AtomicU8::new(0),
            spawned_at_ns: AtomicU64::new(0),
            #[cfg(feature = "test-comm-fault-hook")]
            observation: std::sync::OnceLock::new(),
        }
    }
}

impl BootShared {
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
        let shared = Arc::new(BootShared::default());
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

    /// 只有真实 READY 发送成功，或 standalone 未配置 socket，才能调用。
    pub fn mark_ready(&self) {
        #[cfg(feature = "test-comm-fault-hook")]
        let ready_at = SystemClock::new().now().0;
        self.shared.ready_reached.store(true, Ordering::Release);
        #[cfg(feature = "test-comm-fault-hook")]
        if let Some(page) = self.shared.observation() {
            page.store(
                crate::comm_fault_observation::ObservationField::Ready,
                ready_at,
            );
            page.store(
                crate::comm_fault_observation::ObservationField::ReadyReached,
                1,
            );
        }
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

fn supervise(shared: &BootShared, started_at: MonoRawNs, clock: SystemClock) {
    loop {
        if shared.ready_reached.load(Ordering::Acquire) {
            #[cfg(feature = "test-comm-fault-hook")]
            if let Some(page) = shared.observation() {
                page.store(
                    crate::comm_fault_observation::ObservationField::SupervisorExit,
                    clock.now().0,
                );
            }
            return;
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
                page.store(
                    crate::comm_fault_observation::ObservationField::Fired,
                    now.0,
                );
                page.store(
                    crate::comm_fault_observation::ObservationField::FiredCheckpoint,
                    u64::from(checkpoint),
                );
            }
            // SAFETY: 本路径只终止当前进程，故意不执行可能阻塞的析构或日志。
            unsafe { libc::_exit(88) };
        }
        thread::sleep(Duration::from_millis(50));
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
}
