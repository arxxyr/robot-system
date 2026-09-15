//! 通信故障验收专用接缝，不进入默认构建或生产组合。

mod w4_file_fault;
pub use w4_file_fault::{FileFaultMode, file_fault_from_env, writer_hook};
mod w4_file_kill;
pub use w4_file_kill::{FileKillMode, file_kill_from_env, make_file_kill_hook};
pub mod supervision;
pub mod w4_recovery_kill;

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use iceoryx2::prelude::{NodeBuilder, ServiceName, ZeroCopySend};
use iceoryx2::service::ipc::Service as IxService;
use robot_core::serial_services::BusOpClass;
use robot_core::time::RobotClock;

use crate::worker_stop::WorkerKind;

/// 独立测试 topic 的指令类型，不进入机器人业务 schema。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum HookCommandKind {
    PauseWorker = 1,
    RefuseStop = 2,
    SerialResult = 3,
    RequestWorkerStop = 4,
    ArmSample = 5,
    ExternalEStop = 6,
    NormalDelayProfile = 7,
}

/// 单次脚本返回。通信错误直接沿用 `CommErrorKind` 数值 0..=9、63。
pub const RESULT_HARDWARE_FAULT: u8 = 64;
pub const RESULT_INVALID_PARAMETER: u8 = 65;
pub const RESULT_SUCCESS: u8 = 66;
pub const RESULT_TORQUE_ON: u8 = 67;

/// 跨进程 POD：duration 为相对毫秒，禁止测试端传入绝对时钟。
///
/// Pause/Refuse 的 target 为七 worker 编码；SerialResult 的 target 为
/// `BusOpClass` 编码，count 为 1..=65535 次，error 取上述结果码。
/// `RequestWorkerStop` 只置真实指定 worker 的 stop，duration/count/error 必须为零。
#[repr(C)]
#[derive(Debug, Clone, Copy, ZeroCopySend)]
pub struct TestCommFaultCommand {
    pub sequence: u64,
    pub duration_ms: u32,
    pub count: u32,
    pub kind: u8,
    pub target: u8,
    pub error: u8,
    pub reserved: [u8; 5],
}

const _: () = assert!(std::mem::size_of::<TestCommFaultCommand>() == 24);

struct HookState {
    pause_until: [AtomicU64; 8],
    refuse_until: [AtomicU64; 8],
    results: [AtomicU64; 6],
}

static STATE: HookState = HookState {
    pause_until: [const { AtomicU64::new(0) }; 8],
    refuse_until: [const { AtomicU64::new(0) }; 8],
    results: [const { AtomicU64::new(0) }; 6],
};
static OBSERVATION: std::sync::OnceLock<Arc<crate::comm_fault_observation::ObservationPage>> =
    std::sync::OnceLock::new();
static DELAY_PROFILE: AtomicU64 = AtomicU64::new(0);
static DELAY_SEED: AtomicU64 = AtomicU64::new(1);

/// 只在真实取得 normal 执行权后调用，完整事务仍必须完成，不假造传输响应。
pub(crate) fn delay_acquired_normal(context: crate::comm_fault_trace::TraceContext) {
    let profile = DELAY_PROFILE.load(Ordering::Acquire);
    if profile == 0 {
        return;
    }
    let mut old = DELAY_SEED.load(Ordering::Acquire);
    let random = loop {
        let mut next = old;
        next ^= next << 13;
        next ^= next >> 7;
        next ^= next << 17;
        match DELAY_SEED.compare_exchange(old, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => break next,
            Err(actual) => old = actual,
        }
    };
    let minimum = (profile >> 32) * 1_000_000;
    let maximum = (profile & u64::from(u32::MAX)) * 1_000_000;
    let requested = minimum + random % (maximum - minimum + 1);
    let start = crate::comm_fault_trace::now();
    std::thread::sleep(Duration::from_nanos(requested));
    crate::comm_fault_trace::emit(
        crate::comm_fault_trace::TraceKind::InjectedDelay,
        start,
        context,
        requested,
        crate::comm_fault_trace::now().saturating_sub(start),
    );
}

/// 每个 worker 在正常轮询边界调用；暂停期间仍观察真正 stop，除非明确拒停。
pub fn worker_checkpoint(worker: WorkerKind, clock: &dyn RobotClock, stop: &AtomicBool) {
    while clock.now().0 < STATE.pause_until[worker as usize].load(Ordering::Acquire) {
        if stop.load(Ordering::Acquire) && !refuses_stop(worker, clock) {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[must_use]
pub fn should_stop(worker: WorkerKind, clock: &dyn RobotClock, stop: &AtomicBool) -> bool {
    if !stop.load(Ordering::Acquire) {
        return false;
    }
    if refuses_stop(worker, clock) {
        if let Some(page) = OBSERVATION.get() {
            page.merge_bits(
                crate::comm_fault_observation::ObservationField::RefusedStopBitmap,
                1 << worker as u8,
            );
        }
        false
    } else {
        true
    }
}

fn refuses_stop(worker: WorkerKind, clock: &dyn RobotClock) -> bool {
    clock.now().0 < STATE.refuse_until[worker as usize].load(Ordering::Acquire)
}

pub(crate) fn record_preflight_finished(now: u64) {
    if let Some(page) = OBSERVATION.get() {
        page.store_first(
            crate::comm_fault_observation::ObservationField::PreflightFinished,
            now,
        );
    }
}

pub(crate) struct ScriptedResult {
    pub error: u8,
    pub duration_ms: u32,
}

/// 单词保存返回值和剩余次数；控制器覆盖脚本不会被旧消费 CAS 吞掉。
pub(crate) fn take_serial_result(class: BusOpClass) -> Option<ScriptedResult> {
    let slot = &STATE.results[class as usize];
    let mut observed = slot.load(Ordering::Acquire);
    loop {
        if observed.trailing_zeros() >= 16 {
            return None;
        }
        match slot.compare_exchange(observed, observed - 1, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => {
                return Some(ScriptedResult {
                    error: ((observed >> 16) & 0xff) as u8,
                    duration_ms: (observed >> 32) as u32,
                });
            }
            Err(actual) => observed = actual,
        }
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "同一测试命令的校验、真实动作和对应观测按种类集中处理，保持各拒绝与完成出口"
)]
fn apply_command(
    command: TestCommFaultCommand,
    clock: &dyn RobotClock,
    worker_stops: &[Arc<AtomicBool>; 8],
    external: &crate::event_ingress::ExternalEvents,
    observation: Option<&crate::comm_fault_observation::ObservationPage>,
) -> io::Result<()> {
    if command.kind == HookCommandKind::ArmSample as u8 {
        use crate::comm_fault_trace::{SampleFailure, SampleMode};
        let mode = match command.target {
            0 => Some(SampleMode::ArmInflight),
            1 => Some(SampleMode::Disarm),
            2 => Some(SampleMode::ArmIdle),
            _ => None,
        };
        let accepted = match mode {
            Some(mode)
                if command.reserved == [0; 5]
                    && command.duration_ms == 0
                    && command.count == 0
                    && command.error == 0 =>
            {
                crate::comm_fault_trace::request_boundary(command.sequence, mode, clock.now().0)
            }
            _ => {
                crate::comm_fault_trace::reject_boundary(
                    command.sequence,
                    SampleFailure::InvalidRequest,
                );
                Err(SampleFailure::InvalidRequest)
            }
        };
        if let Err(failure) = accepted {
            // 预期拒绝不能关闭 control：最终独立急停还要经过此线程。
            eprintln!(
                "sample_boundary_rejected sequence={} target={} failure={failure:?}",
                command.sequence, command.target
            );
        }
        return Ok(());
    }
    if command.reserved != [0; 5] {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "测试指令保留位必须为零",
        ));
    }
    let until = if command.duration_ms == 0 {
        0
    } else {
        clock
            .now()
            .0
            .saturating_add(u64::from(command.duration_ms) * 1_000_000)
    };
    match command.kind {
        1 | 2 if command.target < 8 => {
            let slots = if command.kind == 1 {
                &STATE.pause_until
            } else {
                &STATE.refuse_until
            };
            slots[usize::from(command.target)].store(until, Ordering::Release);
        }
        3 if command.target < 6
            && (1..=65535).contains(&command.count)
            && matches!(command.error, 0..=9 | 63..=67) =>
        {
            let word = (u64::from(command.duration_ms) << 32)
                | (u64::from(command.error) << 16)
                | u64::from(command.count);
            STATE.results[usize::from(command.target)].store(word, Ordering::Release);
        }
        4 if command.target < 8
            && command.duration_ms == 0
            && command.count == 0
            && command.error == 0 =>
        {
            worker_stops[usize::from(command.target)].store(true, Ordering::Release);
            if let Some(page) = observation {
                // 只记录本次已实际提交的停止请求，不代表线程已退出。
                page.merge_bits(
                    crate::comm_fault_observation::ObservationField::StopBitmap,
                    1 << command.target,
                );
            }
        }
        6 if command.target == 0
            && command.duration_ms == 0
            && command.count == 0
            && command.error == 0 =>
        {
            external.estop();
        }
        7 if command.target == 0
            && command.error == 0
            && ((command.duration_ms == 0 && command.count == 0)
                || (command.duration_ms >= 1
                    && command.duration_ms <= command.count
                    && command.count <= 5)) =>
        {
            DELAY_SEED.store(command.sequence.max(1), Ordering::Release);
            DELAY_PROFILE.store(
                (u64::from(command.duration_ms) << 32) | u64::from(command.count),
                Ordering::Release,
            );
        }
        _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, "非法测试指令")),
    }
    Ok(())
}

/// 测试注入器不属于七 worker、不参与任何 liveness observer。
pub struct CommFaultControlHandle {
    stop: Arc<AtomicBool>,
    join: JoinHandle<()>,
    ready: crate::ipc_thread::ReadyReader<io::Error>,
}

impl CommFaultControlHandle {
    pub fn poll_ready(&mut self) -> crate::ipc_thread::ReadyState<'_, io::Error> {
        self.ready.poll()
    }
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
    }
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.join.is_finished()
    }
    pub fn join(self) -> std::thread::Result<()> {
        self.join.join()
    }
}

/// 父进程可预先建立同名 service 并发布一条 history 脚本；初始化先 drain 再 ready。
pub fn spawn_control_thread(
    robot_id: String,
    clock: Arc<dyn RobotClock>,
    observation: Option<Arc<crate::comm_fault_observation::ObservationPage>>,
    worker_stops: [Arc<AtomicBool>; 8],
    external: crate::event_ingress::ExternalEvents,
) -> io::Result<CommFaultControlHandle> {
    let expected_initial = std::env::var("ROBOT_TEST_INITIAL_COMMAND_SEQUENCE")
        .ok()
        .map(|value| {
            value
                .parse::<u64>()
                .ok()
                .filter(|sequence| *sequence != 0)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "初始指令序号必须为非零u64")
                })
        })
        .transpose()?;
    if let Some(page) = &observation {
        let _ = OBSERVATION.set(page.clone());
    }
    let stop = Arc::new(AtomicBool::new(false));
    let running_stop = Arc::clone(&stop);
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let join = std::thread::Builder::new()
        .name("s0_comm_fault_control".into())
        .spawn(move || {
            use crate::comm_fault_observation::ObservationField as F;
            let mut ready = Some(ready_tx);
            let result = (|| -> io::Result<()> {
                let node = NodeBuilder::new()
                    .create::<IxService>()
                    .map_err(|e| io::Error::other(format!("{e:?}")))?;
                let name = ServiceName::new(&format!("/{robot_id}/test/comm_fault"))
                    .map_err(|e| io::Error::other(format!("{e:?}")))?;
                let service = node
                    .service_builder(&name)
                    .publish_subscribe::<TestCommFaultCommand>()
                    .max_publishers(1)
                    .max_subscribers(1)
                    .history_size(1)
                    .subscriber_max_buffer_size(8)
                    .enable_safe_overflow(false)
                    .open_or_create()
                    .map_err(|e| io::Error::other(format!("{e:?}")))?;
                let subscriber = service
                    .subscriber_builder()
                    .create()
                    .map_err(|e| io::Error::other(format!("{e:?}")))?;
                let mut initial_applied = expected_initial.is_none();
                loop {
                    while let Some(sample) = subscriber
                        .receive()
                        .map_err(|e| io::Error::other(format!("{e:?}")))?
                    {
                        let command = *sample.payload();
                        apply_command(
                            command,
                            clock.as_ref(),
                            &worker_stops,
                            &external,
                            observation.as_deref(),
                        )?;
                        initial_applied |= expected_initial == Some(command.sequence);
                        if let Some(page) = &observation {
                            page.store(F::CommandApplied, command.sequence);
                        }
                    }
                    if initial_applied && let Some(sender) = ready.take() {
                        if let Some(page) = &observation {
                            page.store(F::HookReady, 1);
                        }
                        let _ = sender.try_send(Ok(()));
                    }
                    if running_stop.load(Ordering::Acquire) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                Ok(())
            })();
            if let Err(error) = result {
                tracing::error!(?error, "通信故障测试控制器退出");
                if let Some(sender) = ready.take() {
                    let _ = sender.try_send(Err(error));
                }
            }
        })?;
    Ok(CommFaultControlHandle {
        stop,
        join,
        ready: crate::ipc_thread::ReadyReader::new(ready_rx),
    })
}

/// 在真实阶段到达后冻结当前线程，保留原监督器与退出路径。
pub fn boot_pause(stage: &str) {
    if std::env::var("ROBOT_TEST_BOOT_PAUSE_STAGE").as_deref() != Ok(stage) {
        return;
    }
    eprintln!("comm_fault_hook stage={stage}");
    loop {
        std::thread::park_timeout(Duration::from_millis(50));
    }
}

/// 原 hook 与 `BootShared` 共用同一观测页；出口处仍由 `RunExit` 持有原残余。
pub(crate) fn record_process_exit_boundary(residual_bitmap: u16) {
    if let Some(page) = OBSERVATION.get() {
        page.record_process_exit(robot_core::time::SystemClock.now().0, residual_bitmap);
    }
}

/// 仅在对应 worker 的真实 spawn 之前注入系统错误，不伪造 daemon 退出码。
///
/// # Errors
///
/// 指定 worker 返回注入错误，其余 worker 不受影响。
pub fn before_spawn(worker: &str) -> io::Result<()> {
    if std::env::var("ROBOT_TEST_BOOT_FAIL_WORKER").as_deref() == Ok(worker) {
        eprintln!("comm_fault_hook fail_worker={worker}");
        return Err(io::Error::other("测试注入 worker spawn 失败"));
    }
    Ok(())
}

/// 只在真实 `FileStep` 公告之后暂停；严格一次匹配，不刷新监督期限。
pub struct WriterStepPause {
    stage: Option<crate::clear_estop_transaction::ClearTxnStage>,
    duration_ns: u64,
    clock: Arc<dyn RobotClock>,
}
impl WriterStepPause {
    /// 未配置时没有暂停；配置非法沿原启动拥有出口返回。
    pub fn from_env(clock: Arc<dyn RobotClock>) -> io::Result<Self> {
        match std::env::var("ROBOT_TEST_WRITER_PAUSE") {
            Err(std::env::VarError::NotPresent) => Ok(Self {
                stage: None,
                duration_ns: 0,
                clock,
            }),
            Err(error) => Err(io::Error::new(io::ErrorKind::InvalidInput, error)),
            Ok(raw) => Self::parse(&raw, clock),
        }
    }
    fn parse(raw: &str, clock: Arc<dyn RobotClock>) -> io::Result<Self> {
        use crate::clear_estop_transaction::ClearTxnStage as S;
        let invalid = || {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "ROBOT_TEST_WRITER_PAUSE 要求真实阶段名:非零 u32 毫秒",
            )
        };
        let (stage, millis) = raw.split_once(':').ok_or_else(invalid)?;
        let stage = match stage {
            "OpenStore" => S::OpenStore,
            "ValidateCredential" => S::ValidateCredential,
            "ReadMarker" => S::ReadMarker,
            "ValidateMarker" => S::ValidateMarker,
            "CreateMarker" => S::CreateMarker,
            "SampleMarkerIdentity" => S::SampleMarkerIdentity,
            "WriteMarker" => S::WriteMarker,
            "SyncMarkerFile" => S::SyncMarkerFile,
            "SyncMarkerDirectory" => S::SyncMarkerDirectory,
            "CreateBackup" => S::CreateBackup,
            "WriteBackup" => S::WriteBackup,
            "SyncBackupFile" => S::SyncBackupFile,
            "RenameBackup" => S::RenameBackup,
            "SyncBackupDirectory" => S::SyncBackupDirectory,
            "CreatePrimary" => S::CreatePrimary,
            "WritePrimary" => S::WritePrimary,
            "SyncPrimaryFile" => S::SyncPrimaryFile,
            "RenamePrimary" => S::RenamePrimary,
            "SyncPrimaryDirectory" => S::SyncPrimaryDirectory,
            "RemoveMarker" => S::RemoveMarker,
            "SyncRemovalDirectory" => S::SyncRemovalDirectory,
            "LoadLatch" => S::LoadLatch,
            "InspectPair" => S::InspectPair,
            _ => return Err(invalid()),
        };
        if millis.is_empty() || !millis.bytes().all(|b| b.is_ascii_digit()) {
            return Err(invalid());
        }
        let millis = millis
            .parse::<std::num::NonZeroU32>()
            .map_err(|_| invalid())?;
        Ok(Self {
            stage: Some(stage),
            duration_ns: u64::from(millis.get()) * 1_000_000,
            clock,
        })
    }
}
impl crate::latch_writer::WriterHook for WriterStepPause {
    fn before_step(&mut self, stage: crate::clear_estop_transaction::ClearTxnStage) {
        if self.stage != Some(stage) {
            return;
        }
        self.stage = None;
        let end = self.clock.now().0.saturating_add(self.duration_ns);
        eprintln!(
            "comm_fault_hook writer_pause_stage={stage:?} duration_ns={}",
            self.duration_ns
        );
        loop {
            let remaining = end.saturating_sub(self.clock.now().0);
            if remaining == 0 {
                break;
            }
            std::thread::sleep(Duration::from_nanos(remaining.min(5_000_000)));
        }
    }
}

/// 实际IPC报告完成后借读同一帧，旧overdue字段仍只表示历史沿。
pub(crate) fn record_current_health(snapshot: &robot_health::RuntimeHealthSnapshot) {
    if let Some(page) = OBSERVATION.get() {
        page.record_current_health(snapshot);
    }
}

#[cfg(test)]
mod writer_pause_tests {
    use super::*;
    #[test]
    fn writer_pause_accepts_only_exact_stage_and_nonzero_u32_millis() {
        let clock: Arc<dyn RobotClock> = Arc::new(robot_core::time::MockClock::new(
            robot_core::time::MonoRawNs(0),
        ));
        for raw in [
            "CreateMarker:1",
            "InspectPair:4294967295",
            "SampleMarkerIdentity:002",
        ] {
            assert!(WriterStepPause::parse(raw, clock.clone()).is_ok(), "{raw}");
        }
        for raw in [
            "create_marker:1",
            "CreateMarker:0",
            "CreateMarker:4294967296",
            "CreateMarker:+1",
            "CreateMarker:1:2",
            "CreateMarker: 1",
            ":1",
            "Open:1",
        ] {
            assert!(WriterStepPause::parse(raw, clock.clone()).is_err(), "{raw}");
        }
    }
}
