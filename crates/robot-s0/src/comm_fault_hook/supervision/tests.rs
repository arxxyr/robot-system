//! 配置反例与F1原线程/文件集成，不冒进程监督或停止后RPC可达性。
use super::*;
use crate::clear_estop_transaction::{ClearTxnStore, GuardInterrupt, StepGuard};
use crate::latch_lock::acquire_latch_lock;
use crate::latch_writer::{
    NoHook, RequestOutcome, Settle, WriterConfig, WriterExit, WriterHandle, WriterPairing, mailbox,
    spawn_writer,
};
use robot_core::time::{MonoRawNs, SystemClock};

#[test]
fn only_fixed_modes_are_accepted_and_old_injection_conflicts_fail_closed() {
    let names = [
        "command-pickup",
        "pre-ready-pickup",
        "pre-ready-file-step",
        "panic-before-primary",
        "panic-after-backup",
        "panic-with-main-held",
        "health-monitor-panic",
        "systemd-watchdog-panic",
    ];
    for (index, name) in names.into_iter().enumerate() {
        assert_eq!(
            parse(Some(name), false).unwrap().unwrap() as usize,
            index + 1
        );
        assert!(parse(Some(name), true).is_err());
    }
    for name in ["", "COMMAND-PICKUP", "pre-ready-pickup:1", "release-ready"] {
        assert!(parse(Some(name), false).is_err());
    }
    assert_eq!(parse(None, true).unwrap(), None);
}

struct Stopped;
impl WriterStop for Stopped {
    fn running(&self) -> bool {
        false
    }
}

struct OriginalWriter(Option<WriterHandle>);
impl OriginalWriter {
    fn join(
        &mut self,
    ) -> (
        thread::Result<WriterExit>,
        crate::latch_writer::WriterJoined,
    ) {
        let until = SystemClock.now().0 + 3_000_000_000;
        loop {
            let finished = self.0.as_ref().unwrap().is_finished();
            if SystemClock.now().0 >= until {
                std::process::abort();
            }
            if finished {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        self.0.take().unwrap().join()
    }
}
impl Drop for OriginalWriter {
    fn drop(&mut self) {
        if self.0.is_some() {
            let _ = self.join();
        }
    }
}

struct ConcurrentRequest {
    shared: Arc<PersistShared>,
    entered: bool,
    before: Vec<ClearTxnStage>,
    after: Vec<ClearTxnStage>,
}
impl StepGuard for ConcurrentRequest {
    fn before(&mut self, stage: ClearTxnStage) -> Result<(), GuardInterrupt> {
        self.before.push(stage);
        Ok(())
    }
    fn after_success(&mut self, stage: ClearTxnStage) {
        self.after.push(stage);
        if stage != ClearTxnStage::SyncBackupDirectory {
            return;
        }
        assert!(!self.entered, "同一原写只命中一次");
        self.entered = true;
        assert!(self.shared.obligation.view().active);
        let shared = self.shared.clone();
        let request = thread::spawn(move || shared.obligation.request(SystemClock.now()));
        let until = SystemClock.now().0 + 3_000_000_000;
        loop {
            let finished = request.is_finished();
            if SystemClock.now().0 >= until {
                std::process::abort();
            }
            if finished {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(request.join().unwrap(), RequestOutcome::Merged);
        let view = self.shared.obligation.view();
        assert!(view.active && view.pending);
    }
}

#[test]
fn original_join_token_real_fallback_callback_concurrent_request_keeps_pending_and_start() {
    let directory = tempfile::tempdir().unwrap();
    let latch = directory.path().join("estop_latch");
    std::fs::write(&latch, b"0\n").unwrap();
    std::fs::write(directory.path().join("estop_latch.bak"), b"0\n").unwrap();
    let lock = acquire_latch_lock(&latch).unwrap();
    let open = || ClearTxnStore::open(&latch, "robot", "arm").unwrap();
    let (slot, mut standby) = WriterPairing::new(open(), open(), &lock, SystemClock.now())
        .unwrap()
        .into_parts();
    let shared = slot.shared().clone();
    let start = SystemClock.now();
    assert_eq!(shared.obligation.request(start), RequestOutcome::Registered);
    let (_client, endpoint) = mailbox(shared.clone());
    let mut writer = OriginalWriter(Some(
        spawn_writer(
            slot,
            endpoint,
            WriterConfig::default(),
            Arc::new(Stopped),
            Arc::new(SystemClock),
            Box::new(NoHook),
        )
        .unwrap(),
    ));
    let (result, joined) = writer.join();
    assert!(matches!(result, Ok(WriterExit::Stopped)));
    assert_eq!(shared.progress.load().unwrap().phase, Phase::Exited);
    let token = standby.after_join(joined).unwrap();
    let claim = shared.obligation.claim_guard().unwrap();
    let mut guard = ConcurrentRequest {
        shared: shared.clone(),
        entered: false,
        before: Vec::new(),
        after: Vec::new(),
    };
    token.persist_once(&mut guard).unwrap();
    assert_eq!(claim.commit(), Settle::PendingRemains);
    assert!(guard.entered);
    assert_eq!(guard.before, guard.after, "每一步均真实成功才记录后回调");
    assert_eq!(guard.after.len(), 10);
    assert_eq!(
        guard.after.last(),
        Some(&ClearTxnStage::SyncBackupDirectory)
    );
    let view = shared.obligation.view();
    assert!(view.pending && !view.active);
    assert_eq!(view.unsettled_since, Some(MonoRawNs(start.0)));
    assert_eq!(std::fs::read(&latch).unwrap(), b"1\n");
    assert_eq!(
        std::fs::read(directory.path().join("estop_latch.bak")).unwrap(),
        b"1\n"
    );
}
