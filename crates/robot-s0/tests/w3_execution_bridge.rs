//! 公开执行器链：父进程有界监督真实 RT 线程，原启动视图与 owner 不可被裸请求替代。

use robot_core::adapter::{
    AdapterInfo, ControlMode, JointCommandView, JointLimits, JointStateBuf, ManipulatorAdapter,
    RtSafeCtrl,
};
use robot_core::error::RobotError;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, atomic::AtomicBool};
use std::time::{Duration, Instant};

#[cfg(feature = "rt_guard")]
#[global_allocator]
static ALLOC_GUARD: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

use robot_adapter_mock::MockHardRTAdapter;
use robot_core::adapter::{JointCommandBuf, RtSnapshot};
use robot_core::control_authority::{
    AuthorityInit, ControlAuthority, InhibitSet, NormalIoIntent, Phase,
};
use robot_core::executor_ctrl::{
    AdapterErr, AdapterNormalReq, AdapterResp, ExecutorCtrlClient, NormalReqStatus,
    OutputSafetyState, SerialEmergencyCtrl,
};
use robot_core::time::{RobotClock, SystemClock};
use robot_s0::hard_rt_ctrl::{HardRtCtrlClient, HardRtCtrlInner};
use robot_s0::rt_thread::{RtThreadConfig, spawn_rt_thread};
use robot_transport::{LatestCommandSlot, RtSnapshotPublisher};

fn wait_until(mut ready: impl FnMut() -> bool) {
    let limit = Instant::now() + Duration::from_secs(2);
    while !ready() {
        assert!(Instant::now() < limit, "真实执行器在测试截止前未完成");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn submitted(client: &HardRtCtrlClient) -> u64 {
    match client.request_normal(AdapterNormalReq::Enable(true)) {
        NormalReqStatus::Submitted { seq } => seq,
        other => panic!("原通道必须接纳此请求：{other:?}"),
    }
}

fn real_executor_child() {
    // 测试主线程断言失败立即结束受监督子进程，栈中唯一活句柄不先被 Drop 遗弃。
    // worker panic 仍正常 unwind，父端必须观察真正 join Err。
    let owner_thread = std::thread::current().id();
    std::panic::set_hook(Box::new(move |info| {
        eprintln!("执行器验收断言：{info}");
        if std::thread::current().id() == owner_thread {
            std::process::exit(101);
        }
    }));
    let authority = Arc::new(
        ControlAuthority::new(AuthorityInit::new(
            false,
            OutputSafetyState::TorqueUnknown,
            InhibitSet::EMPTY,
        ))
        .unwrap(),
    );
    let inner = Arc::new(HardRtCtrlInner::new(authority.clone()));
    let client = HardRtCtrlClient::new(inner.clone());
    let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
    let (_commands, reader) = LatestCommandSlot::new(JointCommandBuf::default());
    let (publisher, _snapshot) = RtSnapshotPublisher::new(RtSnapshot::default());
    let handle = spawn_rt_thread(
        RtThreadConfig {
            strict_rt: false,
            cpu_affinity: None,
            ..RtThreadConfig::default()
        },
        clock,
        MockHardRTAdapter::with_unlimited_limits(2),
        reader,
        publisher,
        inner.clone(),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    wait_until(|| {
        authority.snapshot().safety == OutputSafetyState::TorqueOffConfirmed
            && handle
                .metrics()
                .tick_count
                .load(std::sync::atomic::Ordering::Acquire)
                > 0
    });

    let unowned_seq = submitted(&client);
    wait_until(|| client.poll_last_normal_resp(unowned_seq).is_some());
    assert_eq!(
        client.poll_last_normal_resp(unowned_seq),
        Some(AdapterResp::Err(AdapterErr::AuthorityRevoked))
    );
    assert_eq!(authority.snapshot().phase, Some(Phase::Idle));
    assert!(authority.snapshot().disabled);

    let (owner, view) = authority.reserve_enable().unwrap();
    inner.start_slot().post(view).unwrap();
    let sequence = submitted(&client);
    wait_until(|| client.poll_last_normal_resp(sequence).is_some());
    assert_eq!(
        client.poll_last_normal_resp(sequence),
        Some(AdapterResp::Ok)
    );
    assert!(inner.start_slot().is_empty());
    let finished = authority.snapshot();
    assert_eq!(finished.phase, Some(Phase::EnableSucceeded));
    assert_eq!(finished.safety, OutputSafetyState::TorqueOn);
    assert!(finished.disabled && !finished.normal_io_active);
    assert!(authority.acquire_normal_io(NormalIoIntent::Motion).is_err());
    authority.commit_enable(owner).unwrap();
    assert!(!authority.snapshot().disabled);

    let stops_before = handle
        .metrics()
        .emergency_safety_stop_count
        .load(Ordering::Acquire);
    // 生产 IPC 只发布同 A2 的 EStop；不能靠 safety_stop() 额外写旧 emergency 槽遮住漏接。
    let _ = client.estop_monotonic();
    wait_until(|| {
        authority.snapshot().safety == OutputSafetyState::TorqueOffConfirmed
            && !authority.snapshot().serial_estop_pending
            && handle
                .metrics()
                .emergency_safety_stop_count
                .load(Ordering::Acquire)
                == stops_before + 1
    });
    assert!(
        authority.snapshot().ipc_estop_notice,
        "执行器 claim 不得越权清 IPC 通知"
    );
    assert!(
        matches!(
            authority.reserve_enable(),
            Err(robot_core::control_authority::Refusal::Latched)
        ),
        "mock 不保存第二 latch，重新恢复仍必须通过唯一 A2 的 latch 门"
    );
    handle.request_stop();
    wait_until(|| handle.is_finished());
    let executor_exit = handle.join().unwrap();
    assert!(!executor_exit.has_pending_enable());
}

fn supervised(name: &str, child_body: fn()) {
    const CHILD: &str = "ROBOT_W3_EXECUTION_CHILD";
    if std::env::var(CHILD).ok().as_deref() == Some(name) {
        child_body();
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let log_path = directory.path().join("child.log");
    let log = std::fs::File::create(&log_path).unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(CHILD, name)
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(2)),
            result => {
                let _ = child.kill();
                let reap_deadline = Instant::now() + Duration::from_secs(2);
                loop {
                    if matches!(child.try_wait(), Ok(Some(_))) {
                        break;
                    }
                    // kill 后仍不可回收属于测试进程基础设施失败，不能遗弃句柄继续其它用例。
                    if Instant::now() >= reap_deadline {
                        std::process::abort();
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                panic!("子进程超时或等待错误，已 kill/reap：{result:?}");
            }
        }
    };
    assert!(
        status.success(),
        "真实执行器子进程失败：{}",
        std::fs::read_to_string(log_path).unwrap()
    );
}

#[test]
fn hard_rt_request_requires_original_view_and_real_finish_before_commit() {
    supervised(
        "hard_rt_request_requires_original_view_and_real_finish_before_commit",
        real_executor_child,
    );
}

#[derive(Clone, Copy)]
enum EnableExit {
    Fault,
    Revoked,
    LateOk,
}
struct EnableProbe {
    calls: AtomicU64,
    stop_entered: AtomicBool,
    allow_stop: AtomicBool,
    actual_on: AtomicBool,
}
struct PartialEnableAdapter {
    inner: MockHardRTAdapter,
    authority: Arc<ControlAuthority>,
    observed: Arc<EnableProbe>,
    exit: EnableExit,
    stop_fails: bool,
}
impl ManipulatorAdapter for PartialEnableAdapter {
    fn info(&self) -> AdapterInfo {
        self.inner.info()
    }
    fn connect(&mut self) -> Result<(), RobotError> {
        self.inner.connect()
    }
    fn disconnect(&mut self) -> Result<(), RobotError> {
        self.inner.disconnect()
    }
    fn read_state(&mut self, out: &mut JointStateBuf) -> Result<(), RobotError> {
        self.inner.read_state(out)
    }
    fn write_command(&mut self, cmd: &JointCommandView<'_>) -> Result<(), RobotError> {
        self.inner.write_command(cmd)
    }
    fn set_mode(&mut self, mode: ControlMode) -> Result<(), RobotError> {
        self.inner.set_mode(mode)
    }
    fn enable(&mut self, on: bool) -> Result<(), RobotError> {
        self.enable_rt(on)
    }
    fn safety_stop(&mut self) -> Result<(), RobotError> {
        self.safety_stop_rt()
    }
    fn limits(&self) -> &JointLimits {
        self.inner.limits()
    }
}
impl RtSafeCtrl for PartialEnableAdapter {
    fn enable_rt(&mut self, on: bool) -> Result<(), RobotError> {
        self.inner.enable_rt(on)?;
        self.observed.actual_on.store(on, Ordering::Release);
        if !on {
            return Ok(());
        }
        self.observed.calls.fetch_add(1, Ordering::Release);
        match self.exit {
            EnableExit::Fault => Err(RobotError::HardwareFault("实际 mock 已启用后返回设备失败")),
            EnableExit::Revoked => {
                let _ = self.authority.disable();
                Err(RobotError::Aborted(
                    robot_core::serial_services::AbortReason::Revoked,
                ))
            }
            EnableExit::LateOk => {
                let _ = self.authority.disable();
                Ok(())
            }
        }
    }
    fn safety_stop_rt(&mut self) -> Result<(), RobotError> {
        if self.observed.calls.load(Ordering::Acquire) != 0 {
            self.observed.stop_entered.store(true, Ordering::Release);
            // 只供受父进程截止监督的测试：停在真实返回之前检查旧 Off 已不可用。
            while !self.observed.allow_stop.load(Ordering::Acquire) {
                std::hint::spin_loop();
            }
            if self.stop_fails {
                return Err(RobotError::HardwareFault("真实停止设备失败"));
            }
        }
        self.inner.safety_stop_rt()?;
        self.observed.actual_on.store(false, Ordering::Release);
        Ok(())
    }
    fn set_mode_rt(&mut self, mode: ControlMode) -> Result<(), RobotError> {
        self.inner.set_mode_rt(mode)
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "同一受监督子进程连续验证实际失败与撤销补偿，保持原执行责任到真实 join"
)]
fn compensation_child() {
    let main_thread = std::thread::current().id();
    std::panic::set_hook(Box::new(move |info| {
        eprintln!("Enable 补偿验收失败：{info}");
        if std::thread::current().id() == main_thread {
            std::process::exit(101);
        }
    }));
    for (exit, stop_fails) in [
        (EnableExit::Fault, false),
        (EnableExit::Revoked, false),
        (EnableExit::LateOk, false),
        (EnableExit::Fault, true),
    ] {
        let authority = Arc::new(
            ControlAuthority::new(AuthorityInit::new(
                false,
                OutputSafetyState::TorqueUnknown,
                InhibitSet::EMPTY,
            ))
            .unwrap(),
        );
        let inner = Arc::new(HardRtCtrlInner::new(authority.clone()));
        let client = HardRtCtrlClient::new(inner.clone());
        let observed = Arc::new(EnableProbe {
            calls: AtomicU64::new(0),
            stop_entered: AtomicBool::new(false),
            allow_stop: AtomicBool::new(false),
            actual_on: AtomicBool::new(false),
        });
        let adapter = PartialEnableAdapter {
            inner: MockHardRTAdapter::with_unlimited_limits(2),
            authority: authority.clone(),
            observed: observed.clone(),
            exit,
            stop_fails,
        };
        let (_commands, reader) = LatestCommandSlot::new(JointCommandBuf::default());
        let (publisher, _snapshot) = RtSnapshotPublisher::new(RtSnapshot::default());
        let handle = spawn_rt_thread(
            RtThreadConfig {
                strict_rt: false,
                cpu_affinity: None,
                tick_period_ns: 1_000_000,
                ..RtThreadConfig::default()
            },
            Arc::new(SystemClock::new()),
            adapter,
            reader,
            publisher,
            inner.clone(),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        wait_until(|| {
            handle.metrics().tick_count.load(Ordering::Acquire) > 0
                && authority.snapshot().safety == OutputSafetyState::TorqueOffConfirmed
        });
        let (owner, view) = authority.reserve_enable().unwrap();
        inner.start_slot().post(view).unwrap();
        let seq = submitted(&client);
        wait_until(|| observed.stop_entered.load(Ordering::Acquire));
        let expected = match exit {
            EnableExit::Fault => AdapterResp::Err(AdapterErr::HardwareFault),
            EnableExit::Revoked => AdapterResp::Err(AdapterErr::AuthorityRevoked),
            EnableExit::LateOk => AdapterResp::Ok,
        };
        assert_eq!(client.poll_last_normal_resp(seq), Some(expected));
        assert!(observed.actual_on.load(Ordering::Acquire));
        assert_eq!(
            authority.snapshot().safety,
            OutputSafetyState::StopCommanded
        );
        assert!(authority.acquire_normal_io(NormalIoIntent::Motion).is_err());
        assert!(matches!(
            authority.snapshot().phase,
            Some(Phase::EnableFailed | Phase::EnableSucceeded)
        ));
        observed.allow_stop.store(true, Ordering::Release);
        wait_until(|| {
            handle
                .metrics()
                .emergency_safety_stop_count
                .load(Ordering::Acquire)
                == 2
        });
        assert_eq!(
            authority.snapshot().safety,
            if stop_fails {
                OutputSafetyState::StopFailed
            } else {
                OutputSafetyState::TorqueOffConfirmed
            }
        );
        assert_eq!(observed.actual_on.load(Ordering::Acquire), stop_fails);
        authority.retire_enable(owner).unwrap();
        assert_eq!(
            authority.snapshot().safety,
            if stop_fails {
                OutputSafetyState::StopFailed
            } else {
                OutputSafetyState::TorqueOffConfirmed
            }
        );
        assert_eq!(observed.calls.load(Ordering::Acquire), 1);
        handle.request_stop();
        wait_until(|| handle.is_finished());
        let executor_exit = handle.join().unwrap();
        assert!(!executor_exit.has_pending_enable());
    }
}

#[test]
fn hard_rt_actual_enable_failure_and_revoked_success_stop_before_old_off_can_retire() {
    supervised(
        "hard_rt_actual_enable_failure_and_revoked_success_stop_before_old_off_can_retire",
        compensation_child,
    );
}
