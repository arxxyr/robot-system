//! 五个成功操作后的固定 SIGKILL 观察点；不写权限、原职责或生产期限。

#[cfg(test)]
mod tests;

use std::ffi::OsStr;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use robot_core::time::{MonoRawNs, RobotClock};

use super::w4_file_fault::{command_kind, completion_payload, token_kind};
use crate::boot_supervisor::BootShared;
use crate::clear_estop_transaction::ClearTxnStage as S;
use crate::comm_fault_trace::w4::{self, CommandKind as C, FixtureFailure as F, W4Payload as P};
use crate::latch_writer::{
    Budgets, ClearTxnIdentity, PersistCommand, PersistCompletion, PersistShared, Phase,
    ProgressView, WriterHook, WriterToken,
};

const PAUSE_NS: u64 = 250_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKillMode {
    PrepareCreateMarker,
    PrepareRenameBackup,
    PrepareSyncPrimaryDirectory,
    FinalizeRemoveMarker,
    FinalizeSyncRemovalDirectory,
}

impl FileKillMode {
    const fn point(self) -> (C, S) {
        match self {
            Self::PrepareCreateMarker => (C::Prepare, S::CreateMarker),
            Self::PrepareRenameBackup => (C::Prepare, S::RenameBackup),
            Self::PrepareSyncPrimaryDirectory => (C::Prepare, S::SyncPrimaryDirectory),
            Self::FinalizeRemoveMarker => (C::RemoveCommitted, S::RemoveMarker),
            Self::FinalizeSyncRemovalDirectory => (C::RemoveCommitted, S::SyncRemovalDirectory),
        }
    }
}

/// 资源准备前只读取一次；无配置不接管原 W4a / pause 路径。
///
/// # Errors
/// 值非法、与其它 writer 故障并存，或配置了非 Linux 场景。
pub fn file_kill_from_env() -> io::Result<Option<FileKillMode>> {
    parse_file_kill(
        std::env::var_os("ROBOT_TEST_W4_FILE_KILL").as_deref(),
        std::env::var_os("ROBOT_TEST_WRITER_PAUSE").is_some(),
        std::env::var_os("ROBOT_TEST_W4_FILE_FAULT").is_some(),
        cfg!(target_os = "linux"),
    )
}

fn parse_file_kill(
    raw: Option<&OsStr>,
    old_pause_present: bool,
    file_fault_present: bool,
    linux: bool,
) -> io::Result<Option<FileKillMode>> {
    let Some(raw) = raw else { return Ok(None) };
    if old_pause_present || file_fault_present {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "W4 SIGKILL 不可与其它 writer 故障并存",
        ));
    }
    let mode = match raw.to_str() {
        Some("prepare-create-marker") => FileKillMode::PrepareCreateMarker,
        Some("prepare-rename-backup") => FileKillMode::PrepareRenameBackup,
        Some("prepare-sync-primary-directory") => FileKillMode::PrepareSyncPrimaryDirectory,
        Some("finalize-remove-marker") => FileKillMode::FinalizeRemoveMarker,
        Some("finalize-sync-removal-directory") => FileKillMode::FinalizeSyncRemovalDirectory,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "W4 SIGKILL 只接受五个固定配置值",
            ));
        }
    };
    if !linux {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "W4 SIGKILL 场景仅支持 Linux",
        ));
    }
    Ok(Some(mode))
}

/// 在原 runtime 安装后绑定同一个 writer 与原观测页；只持非 RT Arc。
///
/// # Errors
/// 缺失原安装、writer 不同实例或未配置原观测页。
pub fn make_file_kill_hook(
    mode: FileKillMode,
    clock: Arc<dyn RobotClock>,
    boot: Arc<BootShared>,
    expected_writer: &Arc<PersistShared>,
) -> io::Result<Box<dyn WriterHook>> {
    check_binding(&boot, expected_writer)?;
    Ok(Box::new(FileKillHook::new(mode, clock, boot)))
}

fn check_binding(boot: &BootShared, expected_writer: &Arc<PersistShared>) -> io::Result<()> {
    let binding = boot
        .binding()
        .ok_or_else(|| io::Error::other("W4 SIGKILL 缺少原 runtime 安装"))?;
    if !Arc::ptr_eq(binding.writer(), expected_writer) {
        return Err(io::Error::other("W4 SIGKILL writer 与原 runtime 实例不同"));
    }
    if boot.observation().is_none() {
        return Err(io::Error::other("W4 SIGKILL 缺少原 observation"));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct KillScope {
    source: u64,
    command: C,
    kind_sequence: u64,
    step_seq: u64,
    stage: Option<S>,
    succeeded: bool,
    target_occurrence: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KillWindow {
    entered_at: u64,
    pause_until: u64,
}

fn checked_window(
    progress: ProgressView,
    base: MonoRawNs,
    armed: u64,
    now: MonoRawNs,
) -> Result<KillWindow, F> {
    let entered_at = progress.entered_at.0;
    if progress.phase != Phase::FileStep
        || entered_at <= base.0
        || armed == 0
        || entered_at < armed
        || now.0 < entered_at
    {
        return Err(F::Protocol);
    }
    let deadline = entered_at
        .checked_add(Budgets::default().step.0)
        .ok_or(F::CounterOverflow)?;
    let pause_until = now.0.checked_add(PAUSE_NS).ok_or(F::CounterOverflow)?;
    if pause_until >= deadline {
        return Err(F::Protocol);
    }
    Ok(KillWindow {
        entered_at,
        pause_until,
    })
}

fn shutdown_is_unarmed(boot: &BootShared) -> bool {
    boot.runtime_words().shutdown_deadline().is_none()
}

struct FileKillHook {
    mode: FileKillMode,
    clock: Arc<dyn RobotClock>,
    boot: Arc<BootShared>,
    binding: Option<ClearTxnIdentity>,
    scope: Option<KillScope>,
    execute_sequence: u64,
    prepare_sequence: u64,
    finalize_sequence: u64,
    consumed: bool,
    failed: bool,
}

impl FileKillHook {
    fn new(mode: FileKillMode, clock: Arc<dyn RobotClock>, boot: Arc<BootShared>) -> Self {
        Self {
            mode,
            clock,
            boot,
            binding: None,
            scope: None,
            execute_sequence: 0,
            prepare_sequence: 0,
            finalize_sequence: 0,
            consumed: false,
            failed: false,
        }
    }

    fn emit(&self, payload: P) {
        w4::emit(
            self.scope
                .map_or(self.execute_sequence, |scope| scope.source),
            payload,
        );
    }

    fn fail(&mut self, kind: F) {
        self.failed = true;
        self.emit(P::Failure {
            kind,
            slot: None,
            step_seq: self.scope.map_or(0, |scope| scope.step_seq),
            errno: None,
        });
    }

    #[allow(
        clippy::single_match_else,
        reason = "按项目枚举分派约定穷举递增成功与溢出，保持原单次受检操作"
    )]
    fn next(&mut self, value: u64) -> Option<u64> {
        match value.checked_add(1) {
            Some(next) => Some(next),
            None => {
                self.fail(F::CounterOverflow);
                None
            }
        }
    }

    fn read_window(&self) -> Result<KillWindow, F> {
        let writer = self.boot.binding().ok_or(F::Protocol)?.writer();
        let progress = writer.progress.load().map_err(|_| F::Protocol)?;
        let armed = self
            .boot
            .observation()
            .ok_or(F::Protocol)?
            .snapshot()
            .runtime_armed_ns;
        checked_window(progress, writer.progress.base(), armed, self.clock.now())
    }

    fn pause_after_success(&mut self, scope: KillScope) {
        self.consumed = true;
        let window = match self.read_window() {
            Ok(window) => window,
            Err(reason) => {
                self.fail(reason);
                return;
            }
        };
        // 只证紧邻 Reached 的本次 Acquire 读为 None，不声称未来不能 arm。
        if !shutdown_is_unarmed(&self.boot) || self.clock.now().0 >= window.pause_until {
            self.fail(F::Protocol);
            return;
        }
        self.emit(P::KillReached {
            step_seq: scope.step_seq,
            stage: scope.stage.expect("真实成功回调已核 stage"),
            entered_at: window.entered_at,
            pause_until: window.pause_until,
        });
        loop {
            let now = self.clock.now().0;
            if now >= window.pause_until {
                break;
            }
            std::thread::sleep(Duration::from_nanos(
                (window.pause_until - now).min(1_000_000),
            ));
        }
        // 未被父进程及时杀死是夹具失败；不把成功操作改写为合成 IO 失败。
        self.fail(F::Protocol);
    }
}

impl WriterHook for FileKillHook {
    fn command_begin(&mut self, command: PersistCommand, token: Option<&WriterToken>) {
        if self.scope.is_some() {
            self.fail(F::Protocol);
            self.scope = None;
        }
        let Some(source) = self.next(self.execute_sequence) else {
            return;
        };
        self.execute_sequence = source;
        let kind = command_kind(command);
        let kind_sequence = match kind {
            C::Prepare => {
                let Some(next) = self.next(self.prepare_sequence) else {
                    return;
                };
                self.prepare_sequence = next;
                next
            }
            C::RemoveCommitted => {
                let Some(next) = self.next(self.finalize_sequence) else {
                    return;
                };
                self.finalize_sequence = next;
                next
            }
            _ => source,
        };
        self.scope = Some(KillScope {
            source,
            command: kind,
            kind_sequence,
            step_seq: 0,
            stage: None,
            succeeded: false,
            target_occurrence: 0,
        });
        let txn = command.txn();
        self.emit(P::CommandBegin { txn, command: kind });
        self.emit(P::Token {
            after: false,
            token: token_kind(token),
        });
        match self.binding {
            None if source == 1 && kind == C::Prepare && txn.original_latched => {
                self.binding = Some(txn);
            }
            Some(bound) if bound == txn && kind == C::RemoveCommitted && kind_sequence == 1 => {}
            _ => self.fail(F::Protocol),
        }
    }

    fn command_completed(&mut self, completion: &PersistCompletion, token: Option<&WriterToken>) {
        if self.scope.is_none() {
            self.fail(F::Protocol);
            return;
        }
        self.emit(completion_payload(completion));
        self.emit(P::Token {
            after: true,
            token: token_kind(token),
        });
    }

    fn command_scope_exit(&mut self, completed: bool) {
        let Some(scope) = self.scope else { return };
        if !completed || (scope.command == self.mode.point().0 && !self.consumed) {
            self.fail(F::Protocol);
        }
        self.emit(P::CommandEnd { completed });
        self.scope = None;
    }

    fn outside_command(&mut self) {
        if self.scope.is_some() {
            self.fail(F::Protocol);
            self.scope = None;
        }
    }

    fn before_step(&mut self, stage: S) {
        let Some(mut scope) = self.scope else { return };
        let Some(next) = self.next(scope.step_seq) else {
            return;
        };
        scope.step_seq = next;
        scope.stage = Some(stage);
        scope.succeeded = false;
        if stage == self.mode.point().1 {
            let Some(occurrence) = self.next(scope.target_occurrence) else {
                return;
            };
            scope.target_occurrence = occurrence;
        }
        self.scope = Some(scope);
        self.emit(P::Stage {
            succeeded: false,
            step_seq: next,
            stage,
            occurrence: if stage == self.mode.point().1 {
                scope.target_occurrence
            } else {
                0
            },
        });
    }

    fn after_step_success(&mut self, stage: S) {
        let Some(mut scope) = self.scope else { return };
        if scope.stage != Some(stage) || scope.succeeded {
            self.fail(F::Protocol);
            return;
        }
        scope.succeeded = true;
        self.scope = Some(scope);
        self.emit(P::Stage {
            succeeded: true,
            step_seq: scope.step_seq,
            stage,
            occurrence: if stage == self.mode.point().1 {
                scope.target_occurrence
            } else {
                0
            },
        });
        if !self.failed
            && !self.consumed
            && scope.kind_sequence == 1
            && (scope.command, stage) == self.mode.point()
        {
            self.pause_after_success(scope);
        }
    }
}
