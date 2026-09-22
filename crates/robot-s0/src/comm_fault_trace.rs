//! 测试专用跨进程事件环；记录失败只增加 dropped，绝不参与安全决策。

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};

pub mod w4;

pub const TRACE_CAPACITY: usize = 8192;
const VERSION: u64 = 2;
const RESERVATION_ATTEMPTS: usize = 8;

static TRACE: std::sync::OnceLock<TracePage> = std::sync::OnceLock::new();

#[path = "comm_fault_sample.rs"]
mod sample;
pub use sample::{BoundarySnapshot, SampleFailure, SampleGateRelease, SampleMode, SampleStatus};

#[derive(Default, Clone, Copy)]
pub(crate) struct TraceContext {
    pub source: u64,
    pub normal_seq: u64,
    pub generation: u32,
}

pub(crate) fn now() -> u64 {
    use robot_core::time::{RobotClock, SystemClock};
    SystemClock.now().0
}

pub(crate) fn fault_code(code: crate::state_machine::FaultCode) -> u64 {
    use crate::state_machine::FaultCode;
    match code {
        FaultCode::AdapterErr => 1,
        FaultCode::CmdDeadlineExpired => 2,
        FaultCode::LimitViolation => 3,
        FaultCode::WatchdogTimeout => 4,
        FaultCode::RtOverrun => 5,
        FaultCode::SafetyStopFailed => 6,
        FaultCode::CommFault => 7,
    }
}

/// 安装发生在真实监督线程 spawn 之后；RT 线程从不打开或映射文件。
pub(crate) fn install_from_env() -> io::Result<()> {
    let Some(path) = std::env::var_os("ROBOT_TEST_TRACE_PATH") else {
        return Ok(());
    };
    let page = TracePage::open(Path::new(&path))?;
    TRACE
        .set(page)
        .map_err(|_| io::Error::other("事件环已经安装"))?;
    TRACE.get().expect("刚完成安装").mark_installed();
    Ok(())
}

pub(crate) fn request_boundary(
    sequence: u64,
    mode: SampleMode,
    now: u64,
) -> Result<(), SampleFailure> {
    TRACE
        .get()
        .ok_or(SampleFailure::InvalidRequest)?
        .request_boundary(sequence, mode, now)
}

pub(crate) fn reject_boundary(sequence: u64, failure: SampleFailure) {
    if let Some(page) = TRACE.get() {
        page.reject_boundary(sequence, failure);
    }
}

pub(crate) fn reject_injected_delay() {
    if let Some(page) = TRACE.get() {
        page.reject_injected_delay();
    }
}

impl TracePage {
    /// 未 armed 时也锁存失败；最后 Disarm 后的异常不能等下一次 Arm 才检查。
    pub(crate) fn reject_injected_delay(&self) {
        self.reject_boundary(self.armed_seq(), SampleFailure::PriorFailure);
    }
}

pub(crate) fn serial_service_boundary(
    ctrl: &crate::hard_rt_ctrl::HardRtCtrlInner,
    now: u64,
) -> bool {
    TRACE
        .get()
        .is_some_and(|page| page.service_boundary(ctrl, now))
}

pub(crate) fn sample_gate_active() -> bool {
    TRACE.get().is_some_and(TracePage::sample_gate_active)
}

pub(crate) fn emit(kind: TraceKind, stamp_ns: u64, context: TraceContext, a: u64, b: u64) {
    #[cfg(test)]
    CAPTURE.with(|capture| {
        if let Some(events) = capture.borrow_mut().as_mut() {
            events.push(TraceEvent {
                sample_seq: 1,
                stamp_ns,
                kind,
                source: context.source,
                normal_seq: context.normal_seq,
                generation: context.generation,
                a,
                b,
            });
        }
    });
    if let Some(page) = TRACE.get() {
        let sample_seq = page.armed_seq();
        if sample_seq != 0 {
            page.record(TraceEvent {
                sample_seq,
                stamp_ns,
                kind,
                source: context.source,
                normal_seq: context.normal_seq,
                generation: context.generation,
                a,
                b,
            });
        }
    }
}

// 单测只截取实际 emit 的值；不安装进程全局页、不修改业务状态，也不替代环算法测试。
#[cfg(test)]
std::thread_local! {
    static CAPTURE: std::cell::RefCell<Option<Vec<TraceEvent>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn capture_emitted(body: impl FnOnce()) -> Vec<TraceEvent> {
    struct ResetCapture;
    impl Drop for ResetCapture {
        fn drop(&mut self) {
            CAPTURE.with(|capture| {
                capture.replace(None);
            });
        }
    }
    CAPTURE.with(|capture| assert!(capture.replace(Some(Vec::new())).is_none()));
    let _reset = ResetCapture;
    body();
    CAPTURE.with(|capture| capture.borrow_mut().take().expect("本线程已经开启采集"))
}

/// 以下缓存仅由 serial 写；epoch 不匹配时记录本轮首次真实 typed 通信错误。
static COMM_EPOCH: AtomicU64 = AtomicU64::new(0);
static COMM_STAMP: AtomicU64 = AtomicU64::new(0);
static COMM_SEQ: AtomicU64 = AtomicU64::new(0);
static COMM_KIND: AtomicU64 = AtomicU64::new(0);
static COMM_SOURCE: AtomicU64 = AtomicU64::new(0);

pub(crate) fn comm_error(stamp_ns: u64, context: TraceContext, kind: u8) {
    let Some(page) = TRACE.get() else { return };
    let sample_seq = page.armed_seq();
    if sample_seq == 0 {
        return;
    }
    emit(
        TraceKind::CommErrorObserved,
        stamp_ns,
        context,
        u64::from(kind),
        0,
    );
    if COMM_EPOCH.load(Ordering::Acquire) != sample_seq {
        COMM_STAMP.store(stamp_ns, Ordering::Relaxed);
        COMM_SEQ.store(context.normal_seq, Ordering::Relaxed);
        COMM_KIND.store(u64::from(kind), Ordering::Relaxed);
        COMM_SOURCE.store(context.source, Ordering::Relaxed);
        COMM_EPOCH.store(sample_seq, Ordering::Release);
    }
}

pub(crate) fn bind_comm_to_generation(generation: u32) {
    let Some(page) = TRACE.get() else { return };
    if page.armed_seq() != 0 && COMM_EPOCH.load(Ordering::Acquire) == page.armed_seq() {
        emit(
            TraceKind::FirstCommError,
            COMM_STAMP.load(Ordering::Relaxed),
            TraceContext {
                source: COMM_SOURCE.load(Ordering::Relaxed),
                normal_seq: COMM_SEQ.load(Ordering::Relaxed),
                generation,
            },
            COMM_KIND.load(Ordering::Relaxed),
            0,
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u64)]
pub enum TraceKind {
    TopicReceived = 1,
    ExternalEnter,
    PrimitiveCommitted,
    NormalAcquire,
    NormalRelease,
    SerialClaim,
    IpcClaim,
    SafetyStart,
    SafetyReturn,
    ConfirmReturn,
    AllOffTrusted,
    IpcEStopCommit,
    FirstCommError,
    MailboxPublished,
    FaultIpcCommit,
    InjectedDelay,
    MailboxPublishing,
    CommErrorObserved,
    W4CommandBegin = 19,
    W4TokenBefore = 20,
    W4TokenAfter = 21,
    W4Completion = 22,
    W4CommandEnd = 23,
    W4BeforeEntered = 24,
    W4OperationSucceeded = 25,
    W4FaultSelected = 26,
    W4AliasExpected = 27,
    W4AliasVerified = 28,
    W4DirSyncCall = 29,
    W4DirSyncReturned = 30,
    W4FixtureFailure = 31,
    W4KillReached = 32,
}

impl TraceKind {
    fn decode(value: u64) -> Option<Self> {
        Some(match value {
            1 => Self::TopicReceived,
            2 => Self::ExternalEnter,
            3 => Self::PrimitiveCommitted,
            4 => Self::NormalAcquire,
            5 => Self::NormalRelease,
            6 => Self::SerialClaim,
            7 => Self::IpcClaim,
            8 => Self::SafetyStart,
            9 => Self::SafetyReturn,
            10 => Self::ConfirmReturn,
            11 => Self::AllOffTrusted,
            12 => Self::IpcEStopCommit,
            13 => Self::FirstCommError,
            14 => Self::MailboxPublished,
            15 => Self::FaultIpcCommit,
            16 => Self::InjectedDelay,
            17 => Self::MailboxPublishing,
            18 => Self::CommErrorObserved,
            19 => Self::W4CommandBegin,
            20 => Self::W4TokenBefore,
            21 => Self::W4TokenAfter,
            22 => Self::W4Completion,
            23 => Self::W4CommandEnd,
            24 => Self::W4BeforeEntered,
            25 => Self::W4OperationSucceeded,
            26 => Self::W4FaultSelected,
            27 => Self::W4AliasExpected,
            28 => Self::W4AliasVerified,
            29 => Self::W4DirSyncCall,
            30 => Self::W4DirSyncReturned,
            31 => Self::W4FixtureFailure,
            32 => Self::W4KillReached,

            _ => return None,
        })
    }
}

/// 同一 `sample_seq` 的全部时间戳来自被测 S0 的 `MONOTONIC_RAW`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TraceEvent {
    pub sample_seq: u64,
    pub stamp_ns: u64,
    pub kind: TraceKind,
    pub source: u64,
    pub normal_seq: u64,
    pub generation: u32,
    pub a: u64,
    pub b: u64,
}

#[repr(C, align(64))]
struct Slot {
    generation: AtomicU64,
    fields: [AtomicU64; 8],
}

#[repr(C, align(64))]
struct TraceWords {
    version: AtomicU64,
    head: AtomicU64,
    tail: AtomicU64,
    dropped: AtomicU64,
    consumer_active: AtomicU64,
    installed: AtomicU64,
    armed_seq: AtomicU64,
    boundary: sample::BoundaryWords,
    slots: [Slot; TRACE_CAPACITY],
}

const MAP_BYTES: usize = std::mem::size_of::<TraceWords>().div_ceil(4096) * 4096;

pub struct TracePage {
    mapping: NonNull<TraceWords>,
    _file: File,
}

// SAFETY: 所有共享字段均为原子；映射地址稳定，只有拥有对象最后 Drop 才解除。
unsafe impl Send for TracePage {}
// SAFETY: 每个槽由 head CAS 唯一预留，generation 发布/归还；读取方也有原子准入。
unsafe impl Sync for TracePage {}

impl TracePage {
    /// 父端必须在启动子进程前创建；已有路径不允许覆盖。
    ///
    /// # Errors
    ///
    /// 创建、定长或 mmap 失败时返回对应系统错误。
    pub fn create(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        file.set_len(MAP_BYTES as u64)?;
        let page = Self::map(file)?;
        for (index, slot) in page.words().slots.iter().enumerate() {
            slot.generation.store(index as u64, Ordering::Relaxed);
        }
        page.words().version.store(VERSION, Ordering::Release);
        Ok(page)
    }

    /// 子端只打开已经初始化的页，不创建或截断。
    ///
    /// # Errors
    ///
    /// 版本、长度和底层 I/O 错误均拒绝使用。
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let page = Self::map(file)?;
        if page.words().version.load(Ordering::Acquire) != VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "事件环版本不匹配",
            ));
        }
        Ok(page)
    }

    fn map(file: File) -> io::Result<Self> {
        if file.metadata()?.len() != MAP_BYTES as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "事件环长度不匹配",
            ));
        }
        // SAFETY: fd 已核验长度，页对齐满足 TraceWords 对齐；仅用原子访问共享内存。
        let raw = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                MAP_BYTES,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if raw == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let Some(mapping) = NonNull::new(raw.cast()) else {
            // SAFETY: 成功映射的零地址同样需要归还。
            let _ = unsafe { libc::munmap(raw, MAP_BYTES) };
            return Err(io::Error::other("事件环 mmap 返回空地址"));
        };
        Ok(Self {
            mapping,
            _file: file,
        })
    }

    fn words(&self) -> &TraceWords {
        // SAFETY: mmap 长度/对齐已核验，全零是所有原子的有效表示；借用不越过 Drop。
        unsafe { self.mapping.as_ref() }
    }

    /// 最多八次 CAS 预留，不等待暂停的其它生产者或消费者。
    pub(crate) fn record(&self, event: TraceEvent) {
        self.record_impl(event, || {});
    }

    #[cfg(test)]
    fn record_after_reservation(&self, event: TraceEvent, after_reserve: impl FnOnce()) {
        self.record_impl(event, after_reserve);
    }

    fn record_impl(&self, event: TraceEvent, after_reserve: impl FnOnce()) {
        let words = self.words();
        let mut position = words.head.load(Ordering::Acquire);
        for _ in 0..RESERVATION_ATTEMPTS {
            // 不允许计数器回绕后复用旧代次；实际验收不可能接近此边界。
            if position > u64::MAX - TRACE_CAPACITY as u64 - 1 {
                break;
            }
            let slot = &words.slots
                [usize::try_from(position % TRACE_CAPACITY as u64).expect("槽偏移小于固定容量")];
            let generation = slot.generation.load(Ordering::Acquire);
            if generation < position {
                break;
            }
            if generation != position {
                position = words.head.load(Ordering::Acquire);
                continue;
            }
            match words.head.compare_exchange(
                position,
                position + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    after_reserve();
                    for (field, value) in slot.fields.iter().zip([
                        event.sample_seq,
                        event.stamp_ns,
                        event.kind as u64,
                        event.source,
                        event.normal_seq,
                        u64::from(event.generation),
                        event.a,
                        event.b,
                    ]) {
                        field.store(value, Ordering::Relaxed);
                    }
                    slot.generation.store(position + 1, Ordering::Release);
                    return;
                }
                Err(actual) => position = actual,
            }
        }
        words.dropped.fetch_add(1, Ordering::Relaxed);
    }

    /// 父端批量读；多个读取者误并发调用也不会消费同一槽。
    #[must_use]
    pub fn drain(&self, limit: usize) -> Vec<TraceEvent> {
        let words = self.words();
        if words
            .consumer_active
            .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return Vec::new();
        }
        let _admission = ConsumerAdmission(&words.consumer_active);
        let mut events = Vec::with_capacity(limit.min(TRACE_CAPACITY));
        let mut position = words.tail.load(Ordering::Relaxed);
        for _ in 0..limit.min(TRACE_CAPACITY) {
            let slot = &words.slots
                [usize::try_from(position % TRACE_CAPACITY as u64).expect("槽偏移小于固定容量")];
            if slot.generation.load(Ordering::Acquire) != position + 1 {
                break;
            }
            let values = slot
                .fields
                .each_ref()
                .map(|field| field.load(Ordering::Relaxed));
            if let (Some(kind), Ok(generation)) =
                (TraceKind::decode(values[2]), u32::try_from(values[5]))
            {
                events.push(TraceEvent {
                    sample_seq: values[0],
                    stamp_ns: values[1],
                    kind,
                    source: values[3],
                    normal_seq: values[4],
                    generation,
                    a: values[6],
                    b: values[7],
                });
            } else {
                // 非法事件同样使验收失败，不能静默当作未发生。
                words.dropped.fetch_add(1, Ordering::Relaxed);
            }
            slot.generation
                .store(position + TRACE_CAPACITY as u64, Ordering::Release);
            position += 1;
            words.tail.store(position, Ordering::Release);
        }
        events
    }

    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.words().dropped.load(Ordering::Acquire)
    }

    #[must_use]
    pub fn installed(&self) -> bool {
        self.words().installed.load(Ordering::Acquire) != 0
    }

    pub(crate) fn mark_installed(&self) {
        self.words().installed.store(1, Ordering::Release);
    }

    #[must_use]
    pub fn armed_seq(&self) -> u64 {
        self.words().armed_seq.load(Ordering::SeqCst)
    }

    pub(crate) fn arm(&self, sequence: u64) {
        self.words().armed_seq.store(sequence, Ordering::SeqCst);
    }
}

struct ConsumerAdmission<'a>(&'a AtomicU64);
impl Drop for ConsumerAdmission<'_> {
    fn drop(&mut self) {
        self.0.store(0, Ordering::Release);
    }
}

impl Drop for TracePage {
    fn drop(&mut self) {
        // SAFETY: 对象独占此映射的生命周期，最后一个借用结束后才允许 Drop。
        let _ = unsafe { libc::munmap(self.mapping.as_ptr().cast(), MAP_BYTES) };
    }
}

#[cfg(test)]
#[path = "comm_fault_trace_acceptance.rs"]
mod acceptance;

#[cfg(test)]
mod injected_delay_failure_tests {
    use super::*;

    #[test]
    fn injected_delay_failure_survives_disarm_and_blocks_later_arm() {
        for armed in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let page = TracePage::create(&directory.path().join("trace")).unwrap();
            let (ctrl, _) = crate::post_e_convergence_acceptance::healthy_ipc_control();
            if armed {
                page.request_boundary(10, SampleMode::ArmInflight, 100)
                    .unwrap();
                page.service_boundary(&ctrl, 101);
            }
            page.reject_injected_delay();
            page.request_boundary(11, SampleMode::Disarm, 102).unwrap();
            page.service_boundary(&ctrl, 103);
            let snapshot = page.boundary_snapshot().unwrap();
            assert_eq!(snapshot.status, SampleStatus::Disarmed);
            assert_eq!(snapshot.armed_seq, 0);
            assert_eq!(snapshot.failure, Some(SampleFailure::PriorFailure));
            assert_ne!(snapshot.failure_seq, 0);
            assert!(
                page.request_boundary(12, SampleMode::ArmInflight, 104)
                    .is_err()
            );
        }
    }

    #[test]
    fn injected_delay_failure_after_last_disarm_remains_in_final_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let page = TracePage::create(&directory.path().join("trace")).unwrap();
        let (ctrl, _) = crate::post_e_convergence_acceptance::healthy_ipc_control();
        page.request_boundary(10, SampleMode::Disarm, 100).unwrap();
        page.service_boundary(&ctrl, 101);
        assert_eq!(page.boundary_snapshot().unwrap().failure, None);
        page.reject_injected_delay();
        let final_snapshot = page.boundary_snapshot().unwrap();
        assert_eq!(final_snapshot.status, SampleStatus::Disarmed);
        assert_eq!(final_snapshot.failure, Some(SampleFailure::PriorFailure));
    }
}
