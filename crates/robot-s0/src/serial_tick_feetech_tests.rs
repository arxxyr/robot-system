//! 真实 tick / 工作线程、控制权及 raw adapter 的离线交叉验证；串口使用内存回包。

use super::*;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, atomic::AtomicBool, mpsc};
use std::time::Duration;

use robot_adapter_feetech::adapter::FeetechAdapterConfig;
use robot_adapter_feetech::transport::{
    DrainReport, MockHandle, MockTransport, SourceWriteDeadline,
};
use robot_adapter_feetech::velocity_profile::{
    FirmwareVersion, Sts3215RawWheelProfile, WheelObservation, WheelRole,
};
use robot_adapter_feetech::{
    FeetechAdapter, FeetechBus, FeetechError, Motor, MotorCalibration, SerialTransport, raw_to_rad,
};
use robot_core::adapter::JointCommandBuf;
use robot_core::executor_ctrl::{AdapterNormalReq, ExecutorCtrlClient, NormalReqStatus};
use robot_core::time::{Deadline, DurationNs, MockClock};
use robot_transport::{LatestCommandSlot, RtSnapshotPublisher, RtSnapshotReader};

use crate::config_loader::RobotConfigToml;
use crate::diag::SerialDiagRing;
use crate::grouped_position::XleRobotBus2Layout;
use crate::hard_rt_ctrl::test_authority;
use crate::serial_io_ctrl::SerialIoCtrlClient;
use crate::serial_io_thread::{SerialIoThreadConfig, SerialIoThreadHandle, spawn_serial_io_thread};

const IDS: [u8; 9] = [1, 2, 3, 4, 5, 6, 7, 8, 9];
const ARMS: [u8; 6] = [1, 2, 3, 4, 5, 6];
const WHEELS: [u8; 3] = [8, 7, 9];
const ANCHOR: [u16; 6] = [2010, 2020, 2030, 2040, 2050, 2060];

type Exchanges = Arc<Mutex<VecDeque<(Vec<u8>, Vec<u8>)>>>;

/// 只在匹配的请求写入后投递原帧，避免确认阶段的输入清场提前消费未来响应。
struct Port {
    inner: MockTransport,
    handle: MockHandle,
    exchanges: Exchanges,
    expire_after_write: Arc<AtomicBool>,
    clock: Arc<MockClock>,
}

impl Port {
    fn response(&self, request: &[u8]) -> Vec<u8> {
        let (expected, response) = self
            .exchanges
            .lock()
            .unwrap()
            .pop_front()
            .expect("不得追加未安排的写入、普通读或重试");
        assert_eq!(request, expected, "必须保持原请求顺序、ID分组和字节值");
        response
    }
}

impl SerialTransport for Port {
    fn write_all(&mut self, buf: &[u8]) -> Result<(), FeetechError> {
        let response = self.response(buf);
        self.inner.write_all(buf)?;
        self.handle.push_rx(&response);
        Ok(())
    }

    fn write_all_until(
        &mut self,
        buf: &[u8],
        deadline: SourceWriteDeadline<'_>,
    ) -> Result<(), FeetechError> {
        let response = self.response(buf);
        self.inner.write_all_until(buf, deadline)?;
        self.handle.push_rx(&response);
        if self.expire_after_write.swap(false, Ordering::AcqRel) {
            // 首个GV已完整提交；第二帧必须由原bus/probe截止检查拒绝。
            self.clock.set(deadline.source_deadline().as_mono_raw_ns());
        }
        Ok(())
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), FeetechError> {
        self.inner.read_exact(buf)
    }
    fn set_timeout(&mut self, timeout: Duration) -> Result<(), FeetechError> {
        self.inner.set_timeout(timeout)
    }
    fn is_connected(&self) -> bool {
        self.inner.is_connected()
    }
    fn discard_input(&mut self) -> Result<(), FeetechError> {
        self.inner.discard_input()
    }
    fn drain_until_quiet(
        &mut self,
        quiet_required: Duration,
        absolute_budget: Duration,
    ) -> Result<DrainReport, FeetechError> {
        self.inner
            .drain_until_quiet(quiet_required, absolute_budget)
    }
}

/// 黄金请求不调用生产编码器，独立比对包头、长度、指令、payload及校验和。
fn packet(id: u8, instruction: u8, payload: &[u8]) -> Vec<u8> {
    let mut bytes = vec![
        0xff,
        0xff,
        id,
        u8::try_from(payload.len() + 2).unwrap(),
        instruction,
    ];
    bytes.extend_from_slice(payload);
    bytes.push(
        !bytes[2..]
            .iter()
            .fold(0_u8, |sum, byte| sum.wrapping_add(*byte)),
    );
    bytes
}

fn sync_write(address: u8, width: u8, ids: &[u8], words: &[u16]) -> Vec<u8> {
    assert_eq!(ids.len(), words.len());
    let mut payload = vec![address, width];
    for (&id, word) in ids.iter().zip(words) {
        payload.push(id);
        payload.extend_from_slice(&word.to_le_bytes()[..usize::from(width)]);
    }
    packet(254, 0x83, &payload)
}

struct Fixture {
    clock: Arc<MockClock>,
    ctrl: Arc<SerialIoCtrlInner>,
    probe: Arc<SerialPreemptProbe>,
    diag: SerialDiagRing,
    metrics: SerialIoThreadMetrics,
    tick: SerialTick,
    adapter: Option<FeetechAdapter<Port>>,
    layout: XleRobotBus2Layout,
    exchanges: Exchanges,
    handle: MockHandle,
    expire_after_write: Arc<AtomicBool>,
}

impl Fixture {
    fn new() -> Self {
        let mut fixture = Self::unstarted();
        fixture.boot_and_enable();
        fixture
    }

    fn unstarted() -> Self {
        let mut config: RobotConfigToml =
            toml::from_str(include_str!("../../../configs/xlerobot_bus2_full.toml")).unwrap();
        // 只复用既有名称/ID映射；该历史配置的全Position factory不用于此测试。
        let hardware = config.hardware.remove(0);
        let layout = XleRobotBus2Layout::from_hardware(&hardware).unwrap();
        assert_eq!(hardware.motor_ids, IDS);
        let motors: HashMap<_, _> = hardware
            .joints
            .iter()
            .zip(IDS)
            .map(|(name, id)| (name.clone(), Motor::new(id).unwrap()))
            .collect();
        let calibrations = IDS
            .map(|id| {
                let (min, max) = if id <= 6 { (1000, 3000) } else { (0, 4095) };
                MotorCalibration::new(id, 0, 0, min, max).unwrap()
            })
            .to_vec();
        let clock = Arc::new(MockClock::new(MonoRawNs(100)));
        let ctrl = Arc::new(SerialIoCtrlInner::new_serial(test_authority(true)));
        let probe = Arc::new(SerialPreemptProbe::new(clock.clone(), ctrl.clone()));
        let exchanges = Arc::new(Mutex::new(VecDeque::new()));
        let expire_after_write = Arc::new(AtomicBool::new(false));
        let (inner, handle) = MockTransport::new();
        let bus = FeetechBus::new(
            Port {
                inner,
                handle: handle.clone(),
                exchanges: exchanges.clone(),
                expire_after_write: expire_after_write.clone(),
                clock: clock.clone(),
            },
            motors,
        )
        .unwrap();
        let profiles = [WheelRole::Front, WheelRole::Left, WheelRole::Right].map(|role| {
            Sts3215RawWheelProfile::try_bind(
                WheelObservation {
                    motor_id: role.motor_id(),
                    model: 777,
                    mode: 1,
                    phase: 12,
                    firmware: Some(FirmwareVersion {
                        major: 3,
                        minor: 10,
                    }),
                },
                role,
            )
            .unwrap()
        });
        let adapter = FeetechAdapter::new_bus2_mixed_raw(
            FeetechAdapterConfig {
                clock: clock.clone(),
                preempt: Some(probe.clone()),
                diag: None,
                call_deadline: Duration::from_millis(50),
                adapter_name: "s0_raw_test",
                joint_names: hardware.joints,
                calibrations,
            },
            bus,
            profiles,
        )
        .unwrap();
        Self {
            tick: SerialTick::new(true, 9, clock.now()),
            clock,
            ctrl,
            probe,
            diag: SerialDiagRing::new(),
            metrics: SerialIoThreadMetrics::default(),
            adapter: Some(adapter),
            layout,
            exchanges,
            handle,
            expire_after_write,
        }
    }

    fn enqueue(&self, request: Vec<u8>, response: Vec<u8>) {
        self.exchanges
            .lock()
            .unwrap()
            .push_back((request, response));
    }

    fn read(&self, id: u8, address: u8, response: &[u8]) {
        self.enqueue(
            packet(id, 2, &[address, u8::try_from(response.len()).unwrap()]),
            packet(id, 0, response),
        );
    }

    fn sync_read(&self, address: u8, width: u8, ids: &[u8], words: &[u16]) {
        assert_eq!(ids.len(), words.len());
        let mut payload = vec![address, width];
        payload.extend_from_slice(ids);
        let response = ids
            .iter()
            .zip(words)
            .flat_map(|(&id, word)| packet(id, 0, &word.to_le_bytes()[..usize::from(width)]))
            .collect();
        self.enqueue(packet(254, 0x82, &payload), response);
    }

    fn writes(&self, address: u8, width: u8, ids: &[u8], words: &[u16]) {
        self.enqueue(sync_write(address, width, ids, words), Vec::new());
    }

    fn feedback(&self, arm: [u16; 6], wheel_velocity: u16) -> [u16; 9] {
        let mut positions = [3007, 3008, 3009, 0, 0, 0, 3007, 3008, 3009];
        positions[..6].copy_from_slice(&arm);
        self.sync_read(56, 2, &IDS, &positions);
        self.sync_read(58, 2, &IDS, &[0, 0, 0, 0, 0, 0, 0, 0, wheel_velocity]);
        positions
    }

    fn command(&self, right: i16) -> JointCommandBuf {
        self.layout
            .command(
                ANCHOR.map(|raw| raw_to_rad(i32::from(raw))),
                [0, 0, right],
                77,
                self.clock.now().0,
                self.clock.now().0 + 50_000_000,
            )
            .unwrap()
    }

    fn step(&mut self, command: &JointCommandBuf) {
        self.tick.tick(
            self.adapter.as_mut().unwrap(),
            &command.as_view(),
            &SerialTickContext {
                ctrl: &self.ctrl,
                probe: &self.probe,
                diag: &self.diag,
                clock: self.clock.as_ref(),
                metrics: &self.metrics,
            },
            MonoRawNs(self.clock.now().0 + 10_000_000),
        );
    }

    fn assert_consumed(&self) {
        assert!(
            self.exchanges.lock().unwrap().is_empty(),
            "预定调用必须完整执行"
        );
    }

    fn enqueue_boot(&self) {
        self.writes(40, 1, &IDS, &[0; 9]);
        for id in IDS {
            self.enqueue(packet(id, 1, &[]), packet(id, 0, &[]));
            self.read(id, 3, &777_u16.to_le_bytes());
        }
        for id in IDS {
            self.read(id, 33, &[u8::from(id > 6)]);
            self.read(id, 40, &[0]);
        }
        for id in WHEELS {
            self.read(id, 18, &[12]);
            self.read(id, 0, &[3]);
            self.read(id, 1, &[10]);
        }
        self.sync_read(40, 1, &IDS, &[0; 9]);
    }

    fn enqueue_enable(&self) {
        self.sync_read(56, 2, &ARMS, &ANCHOR);
        self.writes(42, 2, &ARMS, &ANCHOR);
        self.sync_read(42, 2, &ARMS, &ANCHOR);
        self.writes(46, 2, &WHEELS, &[0; 3]);
        for id in IDS {
            self.enqueue(packet(id, 3, &[40, 1]), packet(id, 0, &[]));
        }
        self.feedback(ANCHOR, 0);
    }

    fn boot_and_enable(&mut self) {
        self.enqueue_boot();
        let boot = self.command(0);
        for _ in 0..3 {
            self.step(&boot);
        }
        self.assert_consumed();
        assert_eq!(self.tick.convergence.phase(), ConvergePhase::Idle);
        assert_eq!(
            self.ctrl.safety_state(),
            OutputSafetyState::TorqueOffConfirmed
        );

        // 与IPC既有拥有链一致：预约start-slot，再由SerialTick消费pending ENABLE。
        let client = SerialIoCtrlClient::new(self.ctrl.clone());
        let (owner, view) = self.ctrl.authority().reserve_enable().unwrap();
        client.start_slot().post(view).unwrap();
        let NormalReqStatus::Submitted { seq } =
            client.request_normal(AdapterNormalReq::Enable(true))
        else {
            panic!("真实pending ENABLE必须入队");
        };
        self.enqueue_enable();
        self.step(&boot);
        assert_eq!(client.poll_last_normal_resp(seq), Some(AdapterResp::Ok));
        self.ctrl.authority().commit_enable(owner).unwrap();
        assert_eq!(
            self.adapter.as_ref().unwrap().mixed_arm_anchor().unwrap().0,
            ANCHOR.map(i32::from)
        );
        assert_eq!(
            self.adapter.as_ref().unwrap().mixed_arm_goal_readback(),
            Some(ANCHOR.map(i32::from))
        );
        assert_eq!(
            self.metrics.normal_completed_count.load(Ordering::Relaxed),
            1
        );
        self.assert_consumed();
        let _ = self.handle.drain_tx();
    }

    fn stop_and_confirm(&mut self, command: &JointCommandBuf) {
        self.writes(40, 1, &IDS, &[0; 9]);
        self.sync_read(40, 1, &IDS, &[0; 9]);
        self.step(command);
        assert_eq!(self.tick.convergence.phase(), ConvergePhase::Confirm);
        self.tick.torque_mask[..9].fill(true);
        self.step(command);
        self.assert_consumed();
        assert_eq!(self.tick.convergence.counters().stop_writes, 2);
        assert!(self.tick.torque_mask[..9].iter().all(|on| !on));
        assert_ne!(self.ctrl.safety_state(), OutputSafetyState::TorqueOn);
    }
}

#[test]
fn raw_enable_forward_hold_and_estop_use_real_serial_tick_and_all_nine() {
    let mut fixture = Fixture::new();
    let client = SerialIoCtrlClient::new(fixture.ctrl.clone());
    client.set_policy(OutputPolicy::ActiveForward);
    let command = fixture.command(500);
    fixture.writes(46, 2, &WHEELS, &[0, 0, 500]);
    fixture.writes(42, 2, &ARMS, &ANCHOR);
    let positions = fixture.feedback(ANCHOR.map(|raw| raw + 1), 500);
    fixture.step(&command);
    fixture.assert_consumed();
    let state = fixture.tick.state();
    assert_eq!(state.joint_count, 9);
    for (slot, raw) in positions.into_iter().enumerate() {
        assert_eq!(
            state.positions[slot].to_bits(),
            raw_to_rad(i32::from(raw)).to_bits()
        );
    }
    assert_eq!(
        state.velocities[..9]
            .iter()
            .map(|v| v.to_bits())
            .collect::<Vec<_>>(),
        [0.0_f32, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 500.0].map(f32::to_bits)
    );

    // 真实保持使用刚成功的反馈，轮速先归零，独立预算不改来源目标。
    fixture.clock.advance(DurationNs(10_000_000));
    client.set_policy(OutputPolicy::HoldCurrent);
    fixture.writes(46, 2, &WHEELS, &[0; 3]);
    fixture.writes(42, 2, &ARMS, &ANCHOR.map(|raw| raw + 1));
    fixture.feedback(ANCHOR.map(|raw| raw + 1), 0);
    fixture.step(&command);
    fixture.assert_consumed();
    assert_eq!(command.raw_velocities[8], 500);
    assert_eq!(fixture.metrics.write_errors.load(Ordering::Relaxed), 0);
    assert_eq!(fixture.metrics.read_errors.load(Ordering::Relaxed), 0);
    client.safety_stop();
    fixture.stop_and_confirm(&command);
    assert_eq!(
        fixture.ctrl.safety_state(),
        OutputSafetyState::TorqueOffConfirmed
    );
    assert_eq!(fixture.tick.convergence.counters().confirmed, 2);
}

#[test]
fn raw_expiry_between_gv_and_gp_omits_gp_read_and_retry_then_stops_all_nine() {
    let mut fixture = Fixture::new();
    SerialIoCtrlClient::new(fixture.ctrl.clone()).set_policy(OutputPolicy::ActiveForward);
    let command = fixture.command(500);
    let gv = sync_write(46, 2, &WHEELS, &[0, 0, 500]);
    fixture.enqueue(gv.clone(), Vec::new());
    fixture.expire_after_write.store(true, Ordering::Release);
    fixture.step(&command);
    fixture.assert_consumed();
    assert_eq!(fixture.handle.drain_tx(), gv);
    assert_eq!(fixture.metrics.write_errors.load(Ordering::Relaxed), 1);
    assert_eq!(fixture.metrics.read_errors.load(Ordering::Relaxed), 0);
    assert_eq!(fixture.tick.convergence.phase(), ConvergePhase::StopIssue);
    let fault = fixture
        .ctrl
        .mailbox_snapshot()
        .expect("部分执行必须沿原故障信箱收敛");
    fixture.stop_and_confirm(&command);
    assert_eq!(fixture.ctrl.mailbox_snapshot(), Some(fault));
    assert_ne!(fixture.ctrl.inhibit_bits(), 0);
}

#[test]
fn raw_unobserved_negative_speed_is_rejected_before_first_motion_frame() {
    let mut fixture = Fixture::new();
    SerialIoCtrlClient::new(fixture.ctrl.clone()).set_policy(OutputPolicy::ActiveForward);
    let command = fixture.command(-500);
    fixture.step(&command);
    fixture.assert_consumed();
    assert_eq!(fixture.handle.drain_tx(), Vec::<u8>::new());
    assert_eq!(fixture.metrics.write_errors.load(Ordering::Relaxed), 1);
    assert_eq!(fixture.tick.convergence.phase(), ConvergePhase::StopIssue);
    assert!(fixture.ctrl.mailbox_snapshot().is_some());
    fixture.stop_and_confirm(&command);
}

/// 只在周期边界阻塞工作线程；设备与 probe 仍共用原 `MockClock`。
/// 有界超时仅防测试挂死，测试结果依据握手和快照，不测墙钟调度速度。
struct WorkerClock {
    inner: Arc<MockClock>,
    parked: mpsc::SyncSender<Deadline>,
    release: Mutex<mpsc::Receiver<()>>,
    stop: Arc<AtomicBool>,
}

impl RobotClock for WorkerClock {
    fn now(&self) -> MonoRawNs {
        self.inner.now()
    }

    fn sleep_until(&self, deadline: Deadline) {
        if self.parked.send(deadline).is_err()
            || self
                .release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10))
                .is_err()
        {
            self.stop.store(true, Ordering::Release);
            return;
        }
        self.inner.sleep_until(deadline);
    }

    fn sleep_for(&self, duration: DurationNs) {
        self.inner.sleep_for(duration);
    }
}

/// 测试断言 panic 时也请求退出并唤醒周期门，避免遗留串口工作线程。
struct Worker {
    handle: Option<SerialIoThreadHandle>,
    release: mpsc::SyncSender<()>,
    parked: mpsc::Receiver<Deadline>,
    next_deadline: Deadline,
    snapshot: RtSnapshotReader<RtSnapshot>,
    commands: robot_transport::LatestCommandWriter<JointCommandBuf>,
    metrics: Arc<SerialIoThreadMetrics>,
}

impl Worker {
    fn spawn(fixture: &mut Fixture) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let (parked_tx, parked) = mpsc::sync_channel(1);
        let (release, release_rx) = mpsc::sync_channel(1);
        let clock = Arc::new(WorkerClock {
            inner: fixture.clock.clone(),
            parked: parked_tx,
            release: Mutex::new(release_rx),
            stop: stop.clone(),
        });
        let boot = Arc::new(crate::boot_supervisor::BootShared::new(clock.now()));
        let (commands, reader) = LatestCommandSlot::new(fixture.command(0));
        let (publisher, snapshot) = RtSnapshotPublisher::new(RtSnapshot::default());
        let handle = spawn_serial_io_thread(
            SerialIoThreadConfig::default(),
            clock,
            fixture.adapter.take().unwrap(),
            reader,
            publisher,
            fixture.ctrl.clone(),
            fixture.probe.clone(),
            Arc::new(SerialDiagRing::new()),
            true,
            boot.clone(),
            stop,
        )
        .unwrap();
        let mut worker = Self {
            metrics: handle.metrics().clone(),
            handle: Some(handle),
            release,
            parked,
            next_deadline: Deadline(fixture.clock.now()),
            snapshot,
            commands,
        };
        boot.spawned_at_ns
            .store(fixture.clock.now().0, Ordering::Relaxed);
        boot.spawned_reached.store(true, Ordering::Release);
        worker.await_parked();
        assert_eq!(worker.snapshot.load().tick_seq, 0);
        assert_eq!(fixture.handle.drain_tx(), Vec::<u8>::new());
        worker
    }

    fn await_parked(&mut self) {
        self.next_deadline = self
            .parked
            .recv_timeout(Duration::from_secs(10))
            .expect("真实工作线程必须到达下一个周期门");
    }

    fn step(&mut self) -> RtSnapshot {
        let expected_seq = self.snapshot.load().tick_seq + 1;
        let expected_deadline = self.next_deadline;
        self.release.send(()).unwrap();
        // 原 loop 在发布快照之后才进入下一次 sleep，因此握手也确认发布已完成。
        self.await_parked();
        let snapshot = self.snapshot.load();
        assert_eq!(snapshot.tick_seq, expected_seq);
        assert_eq!(snapshot.tick_deadline_ns, expected_deadline.0.0);
        snapshot
    }

    fn finish(&mut self) {
        self.handle.as_ref().unwrap().request_stop();
        self.release.try_send(()).unwrap();
        let handle = self.handle.take().unwrap();
        let exit = handle.join().unwrap();
        assert!(!exit.has_pending_enable());
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.request_stop();
            let _ = self.release.try_send(());
            // 异常清理不覆盖最初的失败；正常路径由 finish 检查退出结果。
            let _ = handle.join();
        }
    }
}

#[test]
fn raw_worker_consumes_slot_publishes_feedback_and_stops_without_another_tick() {
    let mut fixture = Fixture::unstarted();
    fixture.enqueue_boot();
    let mut worker = Worker::spawn(&mut fixture);
    for _ in 0..3 {
        worker.step();
    }
    fixture.assert_consumed();
    assert_eq!(
        fixture.ctrl.safety_state(),
        OutputSafetyState::TorqueOffConfirmed
    );

    let client = SerialIoCtrlClient::new(fixture.ctrl.clone());
    let (owner, view) = fixture.ctrl.authority().reserve_enable().unwrap();
    client.start_slot().post(view).unwrap();
    let NormalReqStatus::Submitted { seq } = client.request_normal(AdapterNormalReq::Enable(true))
    else {
        panic!("真实 pending ENABLE 必须入队");
    };
    fixture.enqueue_enable();
    worker.step();
    assert_eq!(client.poll_last_normal_resp(seq), Some(AdapterResp::Ok));
    fixture.ctrl.authority().commit_enable(owner).unwrap();
    fixture.assert_consumed();

    client.set_policy(OutputPolicy::ActiveForward);
    worker.commands.store(fixture.command(500));
    fixture.writes(46, 2, &WHEELS, &[0, 0, 500]);
    fixture.writes(42, 2, &ARMS, &ANCHOR);
    let positions = fixture.feedback(ANCHOR.map(|raw| raw + 1), 500);
    let moving = worker.step();
    assert_eq!(moving.state.joint_count, 9);
    assert_eq!(moving.state.stamp_ns, fixture.clock.now().0);
    assert_eq!(
        moving.state.positions[..9]
            .iter()
            .map(|value| value.to_bits())
            .collect::<Vec<_>>(),
        positions.map(|raw| raw_to_rad(i32::from(raw)).to_bits()),
    );
    assert_eq!(
        moving.state.velocities[..9]
            .iter()
            .map(|value| value.to_bits())
            .collect::<Vec<_>>(),
        [0.0_f32, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 500.0].map(f32::to_bits),
    );
    fixture.assert_consumed();

    client.set_policy(OutputPolicy::HoldCurrent);
    fixture.writes(46, 2, &WHEELS, &[0; 3]);
    fixture.writes(42, 2, &ARMS, &ANCHOR.map(|raw| raw + 1));
    fixture.feedback(ANCHOR.map(|raw| raw + 1), 0);
    let holding = worker.step();
    assert_eq!(holding.state.stamp_ns, fixture.clock.now().0);
    assert!(holding.state.stamp_ns > moving.state.stamp_ns);
    assert_eq!(holding.state.velocities[8].to_bits(), 0.0_f32.to_bits());
    fixture.assert_consumed();

    client.safety_stop();
    fixture.writes(40, 1, &IDS, &[0; 9]);
    fixture.sync_read(40, 1, &IDS, &[0; 9]);
    worker.step();
    let stopped = worker.step();
    fixture.assert_consumed();
    assert_eq!(
        fixture.ctrl.safety_state(),
        OutputSafetyState::TorqueOffConfirmed
    );
    assert_eq!(
        worker
            .metrics
            .normal_completed_count
            .load(Ordering::Relaxed),
        1
    );
    assert_eq!(worker.metrics.confirm_off_count.load(Ordering::Relaxed), 2);
    assert_eq!(worker.metrics.read_errors.load(Ordering::Relaxed), 0);
    assert_eq!(worker.metrics.write_errors.load(Ordering::Relaxed), 0);
    assert_eq!(
        worker
            .metrics
            .snapshot_publish_errors
            .load(Ordering::Relaxed),
        0
    );
    let _ = fixture.handle.drain_tx();

    // 工作线程已停在下一个 sleep；退出必须阻止下一 tick 的 IO 和快照发布。
    worker.finish();
    assert_eq!(fixture.handle.drain_tx(), Vec::<u8>::new());
    assert_eq!(worker.metrics.tick_count.load(Ordering::Relaxed), 8);
    assert_eq!(worker.snapshot.load().tick_seq, stopped.tick_seq);
    assert_eq!(
        worker.snapshot.load().tick_deadline_ns,
        stopped.tick_deadline_ns
    );
}
