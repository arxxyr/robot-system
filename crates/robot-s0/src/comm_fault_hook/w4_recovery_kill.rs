//! 启动恢复的单线程、单次成功回调观察；不改文件结果或 CP1 原截止。

#[cfg(test)]
mod tests;

use std::cell::RefCell;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::io::{self, Write};
use std::marker::PhantomData;
use std::rc::Rc;
use std::time::Duration;

use robot_core::time::{MonoRawNs, RobotClock, SystemClock};

use crate::boot_supervisor::PRE_BOOT_BUDGET;
use crate::clear_estop_transaction::{ClearTxnStage as S, FileIdentity, RecoveryObservation};

pub const MAX_RECORD_BYTES: usize = 1024;
const PAUSE_NS: u64 = 250_000_000;
const CONFIGURATION: &str = "repair-rename-primary";
const STAGES: [S; 8] = [
    S::ReadMarker,
    S::ValidateMarker,
    S::SyncMarkerFile,
    S::SyncMarkerDirectory,
    S::CreatePrimary,
    S::WritePrimary,
    S::SyncPrimaryFile,
    S::RenamePrimary,
];
const PREFIX: &str = "ReadMarker,ValidateMarker,SyncMarkerFile,SyncMarkerDirectory,CreatePrimary,WritePrimary,SyncPrimaryFile,RenamePrimary";

#[derive(Clone, Copy)]
enum Phase {
    Waiting,
    Bound {
        observation: RecoveryObservation,
        next: usize,
    },
    Consumed,
    Failed,
}

struct State {
    base: MonoRawNs,
    directory: FileIdentity,
    phase: Phase,
}

thread_local! {
    static RECOVERY: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// 必须留在安装线程；只包住该次原 `recover_and_load` 调用。
///
/// ```compile_fail
/// use robot_s0::comm_fault_hook::w4_recovery_kill::RecoveryScope;
/// fn assert_send<T: Send>() {}
/// assert_send::<RecoveryScope>();
/// ```
#[must_use]
pub struct RecoveryScope {
    _same_thread: PhantomData<Rc<()>>,
}

impl Drop for RecoveryScope {
    fn drop(&mut self) {
        // 先移出并释放 TLS 借用，诊断输出不持内部可变借用。
        let state = RECOVERY.with(|slot| slot.borrow_mut().take());
        if state.is_some_and(|state| matches!(state.phase, Phase::Bound { .. })) {
            diagnostic("原恢复离开时成功步骤前缀未完整");
        }
    }
}

/// 锁与 store 目录匹配后安装，未配置时不改变任何观察状态。
///
/// # Errors
/// 配置非法、与旧故障并存、非 Linux、非法原基准或本线程已安装。
pub fn install_from_env(
    base: MonoRawNs,
    directory: FileIdentity,
) -> io::Result<Option<RecoveryScope>> {
    let enabled = parse_configuration(
        std::env::var_os("ROBOT_TEST_W4_RECOVERY_KILL").as_deref(),
        [
            "ROBOT_TEST_W4_FILE_KILL",
            "ROBOT_TEST_W4_FILE_FAULT",
            "ROBOT_TEST_WRITER_PAUSE",
        ]
        .iter()
        .any(|name| std::env::var_os(name).is_some()),
        cfg!(target_os = "linux"),
    )?;
    if enabled {
        install(base, directory).map(Some)
    } else {
        Ok(None)
    }
}

fn parse_configuration(raw: Option<&OsStr>, conflict: bool, linux: bool) -> io::Result<bool> {
    let Some(raw) = raw else { return Ok(false) };
    if raw != OsStr::new(CONFIGURATION) || conflict {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "恢复 SIGKILL 只接受 repair-rename-primary，且不能与其它 writer 故障并存",
        ));
    }
    if !linux {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "恢复 SIGKILL 仅支持 Linux",
        ));
    }
    Ok(true)
}

fn install(base: MonoRawNs, directory: FileIdentity) -> io::Result<RecoveryScope> {
    if base.0 == 0 || base.0.checked_add(PRE_BOOT_BUDGET.0).is_none() || directory.ino() == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "恢复观察原基准或目录身份无效",
        ));
    }
    RECOVERY.with(|slot| {
        let mut slot = slot.borrow_mut();
        if let Some(state) = slot.as_mut() {
            state.phase = Phase::Failed;
            return Err(io::Error::other("恢复观察拒绝嵌套安装"));
        }
        *slot = Some(State {
            base,
            directory,
            phase: Phase::Waiting,
        });
        Ok(RecoveryScope {
            _same_thread: PhantomData,
        })
    })
}

pub(crate) fn bind_pending(observation: RecoveryObservation) {
    let invalid = RECOVERY.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(state) = slot.as_mut() else {
            return false;
        };
        if !matches!(state.phase, Phase::Waiting)
            || state.directory != observation.directory
            || observation.txn_id == 0
            || observation.marker.ino() == 0
        {
            state.phase = Phase::Failed;
            return true;
        }
        state.phase = Phase::Bound {
            observation,
            next: 0,
        };
        false
    });
    if invalid {
        diagnostic("原 Pending 绑定重复或目录/事务身份不符");
    }
}

#[derive(Clone, Copy)]
struct Publication {
    base: u64,
    reached: u64,
    pause_until: u64,
    observation: RecoveryObservation,
}

fn checked_reached(
    base: MonoRawNs,
    reached: MonoRawNs,
    observation: RecoveryObservation,
) -> io::Result<Publication> {
    let pause_until = reached
        .0
        .checked_add(PAUSE_NS)
        .ok_or_else(|| io::Error::other("恢复暂停截止溢出"))?;
    let cp1 = base
        .0
        .checked_add(PRE_BOOT_BUDGET.0)
        .ok_or_else(|| io::Error::other("原 CP1 截止溢出"))?;
    if base.0 == 0 || reached.0 < base.0 || pause_until >= cp1 {
        return Err(io::Error::other("恢复暂停不能完整落在原 CP1 内"));
    }
    Ok(Publication {
        base: base.0,
        reached: reached.0,
        pause_until,
        observation,
    })
}

pub(crate) fn after_success(completed: S) {
    let action = RECOVERY.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(state) = slot.as_mut() else {
            return Ok(None);
        };
        let Phase::Bound { observation, next } = state.phase else {
            return Ok(None);
        };
        if STAGES.get(next) != Some(&completed) {
            state.phase = Phase::Failed;
            return Err(io::Error::other("恢复成功步骤不符合固定八步前缀"));
        }
        if next + 1 == STAGES.len() {
            state.phase = Phase::Consumed;
            Ok(Some((state.base, observation)))
        } else {
            state.phase = Phase::Bound {
                observation,
                next: next + 1,
            };
            Ok(None)
        }
    });
    // 所有输出、时钟读取和等待都在 TLS 可变借用释放后执行。
    let result = match action {
        Ok(Some((base, observation))) => {
            checked_reached(base, SystemClock.now(), observation).and_then(deliver)
        }
        Ok(None) => return,
        Err(error) => Err(error),
    };
    if let Err(error) = result {
        RECOVERY.with(|slot| {
            if let Some(state) = slot.borrow_mut().as_mut() {
                state.phase = Phase::Failed;
            }
        });
        diagnostic(&error.to_string());
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut value = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        value.push(char::from(DIGITS[usize::from(byte >> 4)]));
        value.push(char::from(DIGITS[usize::from(byte & 15)]));
    }
    value
}

fn record_line(reached: Publication) -> io::Result<String> {
    let observation = reached.observation;
    let mut line = String::with_capacity(MAX_RECORD_BYTES);
    writeln!(&mut line,
        "R4_RECOVERY_REACHED_V1 pid={} base={} reached={} pause_until={} dir_dev={} dir_ino={} marker_dev={} marker_ino={} txn={:032x} epoch={} latched={} record={} checksum={} prefix={PREFIX}",
        std::process::id(), reached.base, reached.reached, reached.pause_until,
        observation.directory.dev(), observation.directory.ino(),
        observation.marker.dev(), observation.marker.ino(), observation.txn_id,
        observation.expected_epoch, u8::from(observation.original_latched),
        hex(&observation.record), hex(&observation.record[80..112]),
    ).map_err(|_| io::Error::other("恢复固定记录格式化失败"))?;
    if line.len() > MAX_RECORD_BYTES {
        return Err(io::Error::other("恢复固定记录超过原长度上限"));
    }
    Ok(line)
}

fn write_record(
    writer: &mut impl Write,
    line: &str,
    reached: Publication,
    clock: &dyn RobotClock,
) -> io::Result<()> {
    writer.write_all(line.as_bytes())?;
    writer.flush()?;
    let now = clock.now().0;
    if now < reached.reached || now >= reached.pause_until {
        return Err(io::Error::other("恢复固定记录写完已不在原暂停窗口内"));
    }
    Ok(())
}

fn deliver(reached: Publication) -> io::Result<()> {
    #[cfg(test)]
    if let Some(output) = tests::take_output() {
        return output(reached);
    }
    let line = record_line(reached)?;
    {
        let stdout = io::stdout();
        let mut writer = stdout.lock();
        write_record(&mut writer, &line, reached, &SystemClock)?;
    }
    loop {
        let now = SystemClock.now().0;
        if now >= reached.pause_until {
            return Err(io::Error::other("恢复原暂停窗口内未被父进程中断"));
        }
        std::thread::sleep(Duration::from_nanos(
            (reached.pause_until - now).min(1_000_000),
        ));
    }
}

fn diagnostic(reason: &str) {
    let _ = writeln!(
        io::stderr().lock(),
        "R4_RECOVERY_DIAGNOSTIC_V1 pid={} reason={reason}",
        std::process::id()
    );
}
