//! 测试专用共享观测页，父进程与 daemon 使用同一 ABI 与原子读取实现。

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};

pub const OBSERVATION_PAGE_BYTES: usize = 4096;
const WORD_COUNT: usize = 26;
const VERSION: u64 = 1;

#[repr(C)]
struct ObservationWords {
    words: [AtomicU64; WORD_COUNT],
}

/// 每个字段独立单调发布；checkpoint/READY 门在对应时刻之后 Release 写入。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObservationSnapshot {
    pub version: u64,
    pub main_start_ns: u64,
    pub spawned_ns: u64,
    pub joint_count: u64,
    pub ready_ns: u64,
    pub supervisor_exit_ns: u64,
    pub fired_ns: u64,
    pub fired_checkpoint: u64,
    pub worker_bitmap: u64,
    pub first_io_ns: u64,
    pub disabled_ns: u64,
    pub stop_bitmap: u64,
    pub preinit_ns: u64,
    pub observation_ready: bool,
    pub first_exit_ns: u64,
    pub ready_reached: bool,
    pub command_applied: u64,
    pub hook_ready: bool,
    pub serial_calls: [u64; 6],
    pub refused_stop_bitmap: u64,
    pub preflight_finished_ns: u64,
}

#[derive(Debug, Clone, Copy)]
#[repr(usize)]
pub(crate) enum ObservationField {
    Version,
    MainStart,
    Spawned,
    JointCount,
    Ready,
    SupervisorExit,
    Fired,
    FiredCheckpoint,
    WorkerBitmap,
    FirstIo,
    Disabled,
    StopBitmap,
    Preinit,
    ObservationReady,
    FirstExit,
    ReadyReached,
    CommandApplied,
    HookReady,
    NormalCalls,
    ReadCalls,
    WriteCalls,
    SafetyCalls,
    ConfirmCalls,
    ConnectCalls,
    RefusedStopBitmap,
    PreflightFinished,
}

pub struct ObservationPage {
    mapping: NonNull<ObservationWords>,
    _file: File,
}

// SAFETY: mapping 始终按页对齐且足以容纳全部原子；仅通过 AtomicU64 访问。
// 映射由对象独占管理，最终 Drop 才解除，因此共享引用期间地址始终稳定。
unsafe impl Send for ObservationPage {}
// SAFETY: 所有跨线程/跨进程可变数据均为原子字段，无普通可变别名。
unsafe impl Sync for ObservationPage {}

impl ObservationPage {
    /// daemon 协作式关停先真实提交 Disabled，再记录此观测时间。
    pub fn record_disabled(&self, now: u64) {
        self.store_first(ObservationField::Disabled, now);
    }
    /// 父端在 spawn 子进程之前创建全零页，路径已存在时拒绝覆盖。
    ///
    /// # Errors
    ///
    /// 文件创建、定长或映射失败返回原始系统错误。
    pub fn create(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        file.set_len(OBSERVATION_PAGE_BYTES as u64)?;
        let page = Self::map(file)?;
        page.store(ObservationField::Version, VERSION);
        Ok(page)
    }

    /// daemon 只打开父端已经建立的观测页，不创建或截断文件。
    ///
    /// # Errors
    ///
    /// 长度、版本或底层系统调用错误均拒绝使用。
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let page = Self::map(file)?;
        if page.load(ObservationField::Version) != VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "观测页版本不匹配",
            ));
        }
        Ok(page)
    }

    fn map(file: File) -> io::Result<Self> {
        if file.metadata()?.len() != OBSERVATION_PAGE_BYTES as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "观测页长度必须为 4096",
            ));
        }
        // SAFETY: fd 有效且长度核验完成；共享读写映射，地址由内核按页对齐。
        let raw = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                OBSERVATION_PAGE_BYTES,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if raw == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let Some(mapping) = NonNull::new(raw.cast::<ObservationWords>()) else {
            // SAFETY: mmap 已成功，即使内核选择零地址，也必须归还此映射。
            let _ = unsafe { libc::munmap(raw, OBSERVATION_PAGE_BYTES) };
            return Err(io::Error::other("观测页 mmap 返回空地址"));
        };
        Ok(Self {
            mapping,
            _file: file,
        })
    }

    fn atomic(&self, field: ObservationField) -> &AtomicU64 {
        // SAFETY: 全零初始化是 AtomicU64 的有效表示，字段在已核长度的映射内。
        unsafe { &self.mapping.as_ref().words[field as usize] }
    }

    pub(crate) fn store(&self, field: ObservationField, value: u64) {
        self.atomic(field).store(value, Ordering::Release);
    }

    pub(crate) fn merge_bits(&self, field: ObservationField, bits: u64) {
        self.atomic(field).fetch_or(bits, Ordering::Release);
    }

    pub(crate) fn store_first(&self, field: ObservationField, value: u64) {
        let _ = self
            .atomic(field)
            .compare_exchange(0, value, Ordering::Release, Ordering::Relaxed);
    }

    pub(crate) fn record_serial_call(
        &self,
        class: robot_core::serial_services::BusOpClass,
        now: u64,
    ) {
        use ObservationField as F;
        use robot_core::serial_services::BusOpClass;
        self.store_first(F::FirstIo, now);
        let field = match class {
            BusOpClass::NormalEffectful => F::NormalCalls,
            BusOpClass::PeriodicRead => F::ReadCalls,
            BusOpClass::PeriodicWrite => F::WriteCalls,
            BusOpClass::Safety => F::SafetyCalls,
            BusOpClass::Confirm => F::ConfirmCalls,
            BusOpClass::Connect => F::ConnectCalls,
        };
        self.atomic(field).fetch_add(1, Ordering::Release);
    }

    fn load(&self, field: ObservationField) -> u64 {
        self.atomic(field).load(Ordering::Acquire)
    }

    #[must_use]
    pub fn snapshot(&self) -> ObservationSnapshot {
        use ObservationField as F;
        // 先读提交门，再读取其之前发布的时间戳。
        let fired_checkpoint = self.load(F::FiredCheckpoint);
        let observation_ready = self.load(F::ObservationReady) != 0;
        let ready_reached = self.load(F::ReadyReached) != 0;
        ObservationSnapshot {
            version: self.load(F::Version),
            main_start_ns: self.load(F::MainStart),
            spawned_ns: self.load(F::Spawned),
            joint_count: self.load(F::JointCount),
            ready_ns: self.load(F::Ready),
            supervisor_exit_ns: self.load(F::SupervisorExit),
            fired_ns: self.load(F::Fired),
            fired_checkpoint,
            worker_bitmap: self.load(F::WorkerBitmap),
            first_io_ns: self.load(F::FirstIo),
            disabled_ns: self.load(F::Disabled),
            stop_bitmap: self.load(F::StopBitmap),
            preinit_ns: self.load(F::Preinit),
            observation_ready,
            first_exit_ns: self.load(F::FirstExit),
            ready_reached,
            command_applied: self.load(F::CommandApplied),
            hook_ready: self.load(F::HookReady) != 0,
            serial_calls: [
                F::NormalCalls,
                F::ReadCalls,
                F::WriteCalls,
                F::SafetyCalls,
                F::ConfirmCalls,
                F::ConnectCalls,
            ]
            .map(|field| self.load(field)),
            refused_stop_bitmap: self.load(F::RefusedStopBitmap),
            preflight_finished_ns: self.load(F::PreflightFinished),
        }
    }
}

impl Drop for ObservationPage {
    fn drop(&mut self) {
        // SAFETY: 此对象拥有这段映射，Drop 后不存在借用；fd 随后自动关闭。
        let _ = unsafe { libc::munmap(self.mapping.as_ptr().cast(), OBSERVATION_PAGE_BYTES) };
    }
}

/// ELF preinit 只记录本进程原始时钟，不依赖 env、日志、堆或共享页。
static PREINIT_NS: AtomicU64 = AtomicU64::new(0);

#[cfg(target_os = "linux")]
extern "C" fn observe_preinit() {
    let mut value = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: 指针指向已初始化且大小正确的 timespec；不访问 Rust runtime。
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC_RAW, &raw mut value) } == 0
        && let (Ok(seconds), Ok(nanos)) =
            (u64::try_from(value.tv_sec), u64::try_from(value.tv_nsec))
    {
        PREINIT_NS.store(
            seconds.saturating_mul(1_000_000_000).saturating_add(nanos),
            Ordering::Relaxed,
        );
    }
}

#[cfg(target_os = "linux")]
#[used]
#[unsafe(link_section = ".preinit_array")]
static PREINIT_OBSERVER: extern "C" fn() = observe_preinit;

pub(crate) fn preinit_ns() -> u64 {
    PREINIT_NS.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_mappings_share_atomic_milestones_and_open_never_truncates() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("observation");
        let parent = ObservationPage::create(&path).unwrap();
        let child = ObservationPage::open(&path).unwrap();
        child.store(ObservationField::MainStart, 123);
        child.store(ObservationField::Fired, 456);
        child.store(ObservationField::FiredCheckpoint, 2);
        let snapshot = parent.snapshot();
        assert_eq!(
            (
                snapshot.main_start_ns,
                snapshot.fired_ns,
                snapshot.fired_checkpoint
            ),
            (123, 456, 2)
        );
        assert!(ObservationPage::create(&path).is_err());
        assert_eq!(ObservationPage::open(&path).unwrap().snapshot(), snapshot);
    }
}
