//! R4c封闭监督模式；只在原writer持有command/义务时观察和有限暂停。

use crate::boot_supervisor::{BootShared, absolute_boot_budget};
use crate::clear_estop_transaction::{ClearTxnStage, DirectorySyncEvent, DirectorySyncInput};
use crate::comm_fault_observation::{ObservationField as F, ObservationPage};
use crate::latch_writer::{
    PersistCommand, PersistCompletion, PersistShared, Phase, WriterHook, WriterStop, WriterToken,
};
use robot_core::time::RobotClock;
use std::io;
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u64)]
pub enum Mode {
    CommandPickup = 1,
    PreReadyPickup = 2,
    PreReadyFileStep = 3,
    PanicBeforePrimary = 4,
    PanicAfterBackup = 5,
    PanicWithMainHeld = 6,
    HealthMonitorPanic = 7,
    SystemdWatchdogPanic = 8,
}
static MODE: OnceLock<Mode> = OnceLock::new();

#[must_use]
pub fn mode() -> Option<Mode> {
    MODE.get().copied()
}

fn parse(value: Option<&str>, conflict: bool) -> io::Result<Option<Mode>> {
    let Some(value) = value else {
        return Ok(None);
    };
    if conflict {
        return Err(io::Error::other("R4c监督模式与旧暂停/文件故障互斥"));
    }
    let mode = match value {
        "command-pickup" => Mode::CommandPickup,
        "pre-ready-pickup" => Mode::PreReadyPickup,
        "pre-ready-file-step" => Mode::PreReadyFileStep,
        "panic-before-primary" => Mode::PanicBeforePrimary,
        "panic-after-backup" => Mode::PanicAfterBackup,
        "panic-with-main-held" => Mode::PanicWithMainHeld,
        "health-monitor-panic" => Mode::HealthMonitorPanic,
        "systemd-watchdog-panic" => Mode::SystemdWatchdogPanic,
        _ => return Err(io::Error::other("未知R4c封闭监督模式")),
    };
    Ok(Some(mode))
}

/// 配置失败发生在原执行资源构造前，不制造义务或凭证。
/// # Errors
/// 非法/冲突配置、重复安装或缺少真实history序号。
pub fn configure_from_env() -> io::Result<()> {
    let value = match std::env::var("ROBOT_TEST_SUP_MODE") {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => return Err(io::Error::other("R4c模式不是UTF-8")),
    };
    let conflicts = [
        "ROBOT_TEST_BOOT_PAUSE_STAGE",
        "ROBOT_TEST_WRITER_PAUSE",
        "ROBOT_TEST_W4_FILE_FAULT",
        "ROBOT_TEST_W4_FILE_KILL",
        "ROBOT_TEST_W4_RECOVERY_KILL",
    ]
    .iter()
    .any(|key| std::env::var_os(key).is_some());
    if let Some(mode) = parse(value.as_deref(), conflicts)? {
        if matches!(mode, Mode::PreReadyPickup | Mode::PreReadyFileStep)
            && std::env::var("ROBOT_TEST_INITIAL_COMMAND_SEQUENCE")
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .is_none_or(|v| v == 0)
        {
            return Err(io::Error::other("READY前模式必须绑定原history的非零序号"));
        }
        MODE.set(mode)
            .map_err(|_| io::Error::other("R4c模式重复安装"))?;
    }
    Ok(())
}

fn failure(page: &ObservationPage, code: u64) {
    page.store_first(F::TestPauseFailure, code);
}
fn startup_deadline(boot: &BootShared, page: &ObservationPage) -> u64 {
    let value = page.snapshot();
    if value.spawned_ns == 0 {
        return boot.started_at().0.saturating_add(5_000_000_000);
    }
    let absolute = boot.started_at().0.saturating_add(
        absolute_boot_budget(boot.joint_count.load(std::sync::atomic::Ordering::Acquire)).0,
    );
    if value.first_exit_ns == 0 {
        absolute
    } else {
        absolute.min(value.first_exit_ns.saturating_add(2_000_000_000))
    }
}

struct Hook {
    inner: Box<dyn WriterHook>,
    mode: Option<Mode>,
    restart_step: bool,
    clock: Arc<dyn RobotClock>,
    boot: Arc<BootShared>,
    shared: Arc<PersistShared>,
    stop: Arc<dyn WriterStop>,
    page: Arc<ObservationPage>,
    obligation_scope: bool,
    selected: bool,
}

/// 包住原hook，仅封闭模式或原S12精确暂停读取真实步骤；不复制token。
/// # Errors
/// 本代原共享页/实例身份缺失。
pub fn wrap_writer(
    inner: Box<dyn WriterHook>,
    clock: Arc<dyn RobotClock>,
    boot: Arc<BootShared>,
    shared: Arc<PersistShared>,
    stop: Arc<dyn WriterStop>,
) -> io::Result<Box<dyn WriterHook>> {
    let mode = mode();
    let restart_step =
        std::env::var("ROBOT_TEST_WRITER_PAUSE").as_deref() == Ok("SyncMarkerFile:4000");
    if mode.is_none() && !restart_step {
        return Ok(inner);
    }
    let page = boot
        .observation_arc()
        .ok_or_else(|| io::Error::other("R4c原观测页缺失"))?;
    if shared.obligation.base() != boot.started_at() {
        return Err(io::Error::other("R4c原义务base不符"));
    }
    page.store(F::SupMode, mode.map_or(0, |m| m as u64));
    Ok(Box::new(Hook {
        inner,
        mode,
        restart_step,
        clock,
        boot,
        shared,
        stop,
        page,
        obligation_scope: false,
        selected: false,
    }))
}

impl Hook {
    fn step(&self, stage: ClearTxnStage) -> bool {
        let Ok(progress) = self.shared.progress.load() else {
            failure(&self.page, 1);
            return false;
        };
        if progress.phase != Phase::FileStep {
            failure(&self.page, 2);
            return false;
        }
        self.page.store(F::StepStage, stage as u64);
        self.page.store(F::StepEntry, progress.entered_at.0);
        self.page.store(F::SupReached, self.clock.now().0);
        true
    }

    fn pause(&self, pickup: bool) {
        loop {
            let now = self.clock.now().0;
            if !self.stop.running() || self.boot.runtime_words().shutdown_deadline().is_some() {
                failure(&self.page, 3);
                return;
            }
            let observed = self.page.snapshot();
            let until = if observed.runtime_armed_ns == 0 {
                startup_deadline(&self.boot, &self.page)
            } else {
                let sup = observed.supervision;
                let source = if pickup {
                    sup.announce_ns
                        .max(sup.pending.since_ns)
                        .max(sup.idle_entry_ns)
                } else {
                    sup.step_entry_ns
                };
                source
                    .max(observed.runtime_armed_ns)
                    .saturating_add(if pickup { 500_000_000 } else { 2_000_000_000 })
                    .saturating_add(260_000_000)
            };
            if now >= until {
                failure(&self.page, 4);
                return;
            }
            thread::sleep(Duration::from_millis(1));
        }
    }
}

impl WriterHook for Hook {
    fn command_received(&mut self, command: &PersistCommand) {
        self.obligation_scope = false;
        self.inner.command_received(command);
        if self.mode != Some(Mode::CommandPickup)
            || self.selected
            || !matches!(command, PersistCommand::PrepareClear { .. })
        {
            return;
        }
        self.selected = true;
        let Ok(progress) = self.shared.progress.load() else {
            failure(&self.page, 5);
            return;
        };
        let Some(announce) = self.shared.announce.offered_at() else {
            failure(&self.page, 6);
            return;
        };
        if progress.phase != Phase::Idle {
            failure(&self.page, 7);
            return;
        }
        let txn = command.txn().txn_id;
        self.page.store(
            F::CommandTxnLow,
            u64::try_from(txn & u128::from(u64::MAX)).expect("已限定低64位"),
        );
        self.page.store(F::CommandTxnHigh, (txn >> 64) as u64);
        self.page.store(F::AnnounceAt, announce.0);
        self.page.store(F::IdleEntry, progress.entered_at.0);
        let now = self.clock.now().0;
        self.page.store(F::CommandReached, now);
        self.page.store(F::SupReached, now);
        self.pause(true);
    }

    fn before_obligation_claim(&mut self) {
        self.inner.before_obligation_claim();
        if !matches!(
            self.mode,
            Some(
                Mode::PreReadyPickup
                    | Mode::PreReadyFileStep
                    | Mode::PanicBeforePrimary
                    | Mode::PanicAfterBackup
                    | Mode::PanicWithMainHeld
            )
        ) || self.selected
        {
            return;
        }
        self.obligation_scope = true;
        let Ok(progress) = self.shared.progress.load() else {
            failure(&self.page, 8);
            return;
        };
        self.page.store(F::IdleEntry, progress.entered_at.0);
        self.page.record_pending(&self.shared, self.clock.now().0);
        if self.mode == Some(Mode::PreReadyPickup) {
            self.selected = true;
            if progress.phase != Phase::Idle || self.shared.announce.offered_at().is_some() {
                failure(&self.page, 9);
                return;
            }
            self.page.store(F::SupReached, self.clock.now().0);
            self.pause(true);
        }
    }

    fn before_step(&mut self, stage: ClearTxnStage) {
        if !self.selected && self.restart_step && stage == ClearTxnStage::SyncMarkerFile {
            self.selected = true;
            self.step(stage);
        }
        if !self.selected && self.obligation_scope && stage == ClearTxnStage::CreatePrimary {
            self.page.record_active(&self.shared, self.clock.now().0);
            match self.mode {
                Some(Mode::PreReadyFileStep) => {
                    self.selected = true;
                    if self.step(stage) {
                        self.pause(false);
                    }
                }
                Some(Mode::PanicBeforePrimary | Mode::PanicWithMainHeld) => {
                    self.selected = true;
                    assert!(!self.step(stage), "R4c原ClaimGuard内CreatePrimary前panic");
                }
                _ => {}
            }
        }
        self.inner.before_step(stage);
    }

    fn after_step_success(&mut self, stage: ClearTxnStage) {
        self.inner.after_step_success(stage);
        if self.obligation_scope
            && !self.selected
            && self.mode == Some(Mode::PanicAfterBackup)
            && stage == ClearTxnStage::SyncBackupDirectory
        {
            self.selected = true;
            assert!(!self.step(stage), "R4c原ClaimGuard内最后文件成功后panic");
        }
    }
    fn outside_command(&mut self) {
        self.obligation_scope = false;
        self.inner.outside_command();
    }
    fn directory_sync_input(&mut self, stage: ClearTxnStage) -> DirectorySyncInput {
        self.inner.directory_sync_input(stage)
    }
    fn directory_sync_event(&mut self, stage: ClearTxnStage, event: DirectorySyncEvent) {
        self.inner.directory_sync_event(stage, event);
    }
    fn command_begin(&mut self, command: PersistCommand, token: Option<&WriterToken>) {
        self.inner.command_begin(command, token);
    }
    fn command_completed(&mut self, completion: &PersistCompletion, token: Option<&WriterToken>) {
        self.inner.command_completed(completion, token);
    }
    fn command_scope_exit(&mut self, completed: bool) {
        self.inner.command_scope_exit(completed);
    }
}

/// 原 `run_workers` 成功后、`send_ready` 之前最多等250ms，不能借用CP2收尾余量。
/// # Errors
/// 真实原件未到、原门超时/退出/绑定不符。
pub fn before_ready(
    boot: &BootShared,
    shared: &PersistShared,
    clock: &dyn RobotClock,
    failed: impl Fn() -> bool,
) -> Result<(), i32> {
    let Some(mode @ (Mode::PreReadyPickup | Mode::PreReadyFileStep)) = mode() else {
        return Ok(());
    };
    let page = boot.observation().ok_or(1)?;
    let until = clock
        .now()
        .0
        .saturating_add(250_000_000)
        .min(startup_deadline(boot, page));
    let expected = std::env::var("ROBOT_TEST_INITIAL_COMMAND_SEQUENCE")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or(1)?;
    loop {
        if clock.now().0 >= until || failed() || boot.runtime_words().shutdown_deadline().is_some()
        {
            failure(page, 10);
            return Err(1);
        }
        let observed = page.snapshot();
        let sup = observed.supervision;
        if sup.reached_ns != 0 {
            let view = shared.obligation.view();
            let phase = shared.progress.load().map_err(|_| 1)?.phase;
            let correct = match mode {
                Mode::PreReadyPickup => view.pending && !view.active && phase == Phase::Idle,
                Mode::PreReadyFileStep => view.active && phase == Phase::FileStep,
                _ => false,
            };
            if clock.now().0 >= until
                || !correct
                || observed.ready_reached
                || observed.runtime_armed_ns != 0
                || observed.command_applied != expected
                || !observed.hook_ready
                || sup.obligation_id != shared.obligation.id().get()
                || sup.pending.at_ns == 0
                || sup.pause_failure != 0
            {
                failure(page, 11);
                return Err(1);
            }
            return Ok(());
        }
        thread::sleep(Duration::from_millis(1));
    }
}

/// 已安装全部原helper后，S6只有限挡main；不生成D或消费原writer句柄。
/// # Errors
/// 原RuntimeArmed/Exited未在有限同步窗到达，或监督未按原候选退出。
pub fn hold_main(
    boot: &BootShared,
    shared: &PersistShared,
    clock: &dyn RobotClock,
) -> Result<(), i32> {
    if mode() != Some(Mode::PanicWithMainHeld) {
        return Ok(());
    }
    let page = boot.observation().ok_or(1)?;
    let until = clock.now().0.saturating_add(3_000_000_000);
    let armed = loop {
        let now = clock.now().0;
        let observed = page.snapshot();
        if now >= until || boot.runtime_words().shutdown_deadline().is_some() {
            failure(page, 12);
            return Err(1);
        }
        if observed.runtime_armed_ns != 0 {
            if clock.now().0 >= until {
                failure(page, 12);
                return Err(1);
            }
            break observed.runtime_armed_ns;
        }
        thread::sleep(Duration::from_millis(1));
    };
    page.store(F::MainHold, clock.now().0);
    loop {
        let now = clock.now().0;
        if now >= until || boot.runtime_words().shutdown_deadline().is_some() {
            failure(page, 13);
            return Err(1);
        }
        let progress = shared.progress.load().map_err(|_| 1)?;
        if progress.phase == Phase::Exited {
            let observed_at = clock.now().0;
            if observed_at >= until {
                failure(page, 13);
                return Err(1);
            }
            page.record_writer_exited_view(shared, observed_at);
            let end = progress
                .entered_at
                .0
                .max(armed)
                .saturating_add(500_000_000)
                .saturating_add(260_000_000);
            while clock.now().0 < end {
                thread::sleep(Duration::from_millis(1));
            }
            failure(page, 14);
            return Err(1);
        }
        thread::sleep(Duration::from_millis(1));
    }
}

pub fn main_observed_writer(boot: &BootShared, shared: &PersistShared, clock: &dyn RobotClock) {
    if let Some(page) = boot.observation() {
        let now = clock.now().0;
        page.store_first(F::MainWriterObserved, now);
        if shared
            .progress
            .load()
            .is_ok_and(|v| v.phase == Phase::Exited)
        {
            page.record_writer_exited_view(shared, now);
        }
    }
}

#[cfg(test)]
mod tests;
