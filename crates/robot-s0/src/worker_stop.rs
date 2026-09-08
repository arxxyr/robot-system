//! 七个业务 worker 共享的预建停止标志与成功 spawn 位图。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

const WORKER_COUNT: usize = 7;
const ALL_SPAWNED: u8 = (1 << WORKER_COUNT) - 1;

/// 生产只读停止位；验收组合在相同循环边界施加暂停或拒停。
pub(crate) fn worker_running(
    worker: WorkerKind,
    clock: &dyn robot_core::time::RobotClock,
    stop: &AtomicBool,
) -> bool {
    #[cfg(feature = "test-comm-fault-hook")]
    {
        crate::comm_fault_hook::worker_checkpoint(worker, clock, stop);
        !crate::comm_fault_hook::should_stop(worker, clock, stop)
    }
    #[cfg(not(feature = "test-comm-fault-hook"))]
    {
        let _ = (worker, clock);
        !stop.load(Ordering::Acquire)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum WorkerKind {
    Ipc,
    ClearEStopRr,
    SetEnabledRr,
    Watchdog,
    Executor,
    Pub,
    Io,
}

impl WorkerKind {
    pub const ALL: [Self; WORKER_COUNT] = [
        Self::Ipc,
        Self::ClearEStopRr,
        Self::SetEnabledRr,
        Self::Watchdog,
        Self::Executor,
        Self::Pub,
        Self::Io,
    ];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Ipc => "ipc",
            Self::ClearEStopRr => "clear_rr",
            Self::SetEnabledRr => "set_rr",
            Self::Watchdog => "watchdog",
            Self::Executor => "executor",
            Self::Pub => "pub",
            Self::Io => "io",
        }
    }
}

pub struct StopFlags {
    flags: [Arc<AtomicBool>; WORKER_COUNT],
    spawned: AtomicU8,
    #[cfg(feature = "test-comm-fault-hook")]
    observation: std::sync::OnceLock<Arc<crate::comm_fault_observation::ObservationPage>>,
}

impl Default for StopFlags {
    fn default() -> Self {
        Self::new()
    }
}

impl StopFlags {
    #[must_use]
    pub fn new() -> Self {
        Self {
            flags: std::array::from_fn(|_| Arc::new(AtomicBool::new(false))),
            spawned: AtomicU8::new(0),
            #[cfg(feature = "test-comm-fault-hook")]
            observation: std::sync::OnceLock::new(),
        }
    }

    #[must_use]
    pub fn flag(&self, worker: WorkerKind) -> Arc<AtomicBool> {
        Arc::clone(&self.flags[worker as usize])
    }

    /// 仅由主线程在成功得到对应 handle 后调用。
    pub fn mark_spawned(&self, worker: WorkerKind) {
        self.spawned.fetch_or(1 << worker as u8, Ordering::Release);
        #[cfg(feature = "test-comm-fault-hook")]
        if let Some(page) = self.observation.get() {
            page.merge_bits(
                crate::comm_fault_observation::ObservationField::WorkerBitmap,
                1 << worker as u8,
            );
        }
    }

    /// 不依赖工作线程配合，只给已启动成员一次性置停止位。
    pub fn request_spawned_stop(&self) {
        let spawned = self.spawned.load(Ordering::Acquire);
        for worker in WorkerKind::ALL {
            if spawned & (1 << worker as u8) != 0 {
                self.request_worker_stop(worker);
            }
        }
    }

    /// 顺序关闭时也使用同一真实停止原子，并在写入后记录停止请求。
    pub fn request_worker_stop(&self, worker: WorkerKind) {
        self.flags[worker as usize].swap(true, Ordering::AcqRel);
        #[cfg(feature = "test-comm-fault-hook")]
        if let Some(page) = self.observation.get() {
            page.merge_bits(
                crate::comm_fault_observation::ObservationField::StopBitmap,
                1 << worker as u8,
            );
        }
    }

    #[cfg(feature = "test-comm-fault-hook")]
    pub fn set_observation(&self, page: Arc<crate::comm_fault_observation::ObservationPage>) {
        let _ = self.observation.set(page);
    }

    #[must_use]
    pub fn all_spawned(&self) -> bool {
        self.spawned.load(Ordering::Acquire) == ALL_SPAWNED
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_only_reaches_spawned_workers_and_all_seven_are_required() {
        let flags = StopFlags::new();
        for worker in WorkerKind::ALL.into_iter().take(6) {
            flags.mark_spawned(worker);
        }
        assert!(!flags.all_spawned());
        flags.request_spawned_stop();
        for worker in WorkerKind::ALL {
            assert_eq!(
                flags.flag(worker).load(Ordering::Acquire),
                worker != WorkerKind::Io
            );
        }
        flags.mark_spawned(WorkerKind::Io);
        assert!(flags.all_spawned());
        flags.request_spawned_stop();
        assert!(flags.flag(WorkerKind::Io).load(Ordering::Acquire));
    }
}
