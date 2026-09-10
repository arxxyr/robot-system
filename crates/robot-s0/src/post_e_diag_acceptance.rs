//! AC-10 独立验收：真实串口 tick / Feetech 重试产生固定诊断，不在串口线程发 tracing。
//!
//! 脚本只规定设备回包和 transport 错误；调度、重试、恢复和故障收敛全部调用生产实现。

#![cfg(feature = "feetech")]

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use robot_adapter_feetech::adapter::FeetechAdapterConfig;
use robot_adapter_feetech::transport::DrainReport;
use robot_adapter_feetech::{FeetechAdapter, FeetechBus, FeetechError, Motor, SerialTransport};
use robot_core::adapter::JointCommandBuf;
use robot_core::diag::DiagEvent;
use robot_core::executor_ctrl::{
    AdapterErr, AdapterNormalReq, AdapterResp, ExecutorCtrlClient, NormalReqStatus,
    OutputSafetyState,
};
use robot_core::serial_services::CommErrorKind;
use robot_core::time::{DurationNs, MockClock, MonoRawNs, RobotClock};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

use crate::diag::SerialDiagRing;
use crate::hard_rt_ctrl::{HardRtCtrlClient, HardRtCtrlInner};
use crate::serial_convergence::{BootOutcome, ConvergePhase};
use crate::serial_io_thread::{SerialIoThreadMetrics, SerialTick, SerialTickContext};
use crate::serial_preempt::SerialPreemptProbe;
use crate::state_machine::FaultCode;

const SERIAL_THREAD: &str = "s0_serial_io_thread";
const CANARY_TARGET: &str = "post_e_diag_acceptance_canary";
const MOTOR: u8 = 42;
const INITIAL_NS: u64 = 1_000_000_000;

#[derive(Default)]
struct TraceCounts {
    canary: AtomicUsize,
    business_events: AtomicUsize,
    business_spans: AtomicUsize,
    wrong_thread: AtomicUsize,
}

struct LocalSubscriber(Arc<TraceCounts>);

impl LocalSubscriber {
    fn check_thread(&self) {
        if std::thread::current().name() != Some(SERIAL_THREAD) {
            self.0.wrong_thread.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl Subscriber for LocalSubscriber {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _span: &Attributes<'_>) -> Id {
        self.check_thread();
        let count = self.0.business_spans.fetch_add(1, Ordering::Relaxed);
        Id::from_u64(u64::try_from(count).unwrap() + 1)
    }

    fn record(&self, _span: &Id, _values: &Record<'_>) {}
    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}
    fn enter(&self, _span: &Id) {}
    fn exit(&self, _span: &Id) {}

    fn event(&self, event: &Event<'_>) {
        self.check_thread();
        let counter = match event.metadata().target() {
            CANARY_TARGET => &self.0.canary,
            _ => &self.0.business_events,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

fn in_serial_thread<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> (T, Arc<TraceCounts>) {
    let counts = Arc::new(TraceCounts::default());
    let captured = counts.clone();
    let result = std::thread::Builder::new()
        .name(SERIAL_THREAD.to_owned())
        .spawn(move || {
            tracing::subscriber::with_default(LocalSubscriber(captured.clone()), || {
                assert_eq!(std::thread::current().name(), Some(SERIAL_THREAD));
                tracing::warn!(target: "post_e_diag_acceptance_canary", phase = "before");
                assert_eq!(
                    captured.canary.load(Ordering::Relaxed),
                    1,
                    "前置 canary 必须真实可见"
                );
                let result = work();
                tracing::warn!(target: "post_e_diag_acceptance_canary", phase = "after");
                assert_eq!(
                    captured.canary.load(Ordering::Relaxed),
                    2,
                    "业务区后订阅器仍须生效"
                );
                result
            })
        })
        .unwrap()
        .join()
        .unwrap();
    assert_eq!(counts.wrong_thread.load(Ordering::Relaxed), 0);
    (result, counts)
}

/// 返回包在对应请求发出时才进入 RX；hygiene 不会丢掉尚未发送请求的未来回包。
enum Reply {
    Bytes(Vec<u8>),
    NoResponseRequired,
    ReadTimeout,
    WriteFailure,
}

struct WireStep {
    tx: Vec<u8>,
    reply: Reply,
}

struct ScriptedTransport {
    steps: VecDeque<WireStep>,
    pending_rx: Option<Reply>,
    clock: Arc<MockClock>,
    writes: Arc<AtomicUsize>,
}

impl SerialTransport for ScriptedTransport {
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), FeetechError> {
        assert!(self.pending_rx.is_none(), "上一事务的回包尚未消费");
        let step = self
            .steps
            .pop_front()
            .expect("真实实现发出了脚本外的额外 TX");
        assert_eq!(bytes, step.tx, "实际请求必须匹配独立设备脚本");
        self.writes.fetch_add(1, Ordering::Relaxed);
        self.clock.advance(DurationNs::from_millis(1));
        match step.reply {
            Reply::WriteFailure => Err(FeetechError::TransportWrite(std::io::Error::other(
                "脚本 TX 失败",
            ))),
            Reply::NoResponseRequired => Ok(()),
            reply => {
                self.pending_rx = Some(reply);
                Ok(())
            }
        }
    }

    fn read_exact(&mut self, bytes: &mut [u8]) -> Result<(), FeetechError> {
        self.clock.advance(DurationNs::from_millis(1));
        match self.pending_rx.take().expect("没有对应 TX 的 RX 调用") {
            Reply::Bytes(reply) => {
                assert_eq!(bytes.len(), reply.len(), "真实读取长度必须匹配完整设备响应");
                bytes.copy_from_slice(&reply);
                Ok(())
            }
            Reply::ReadTimeout => Err(FeetechError::TransportTimeout {
                timeout_ms: 50,
                wanted: bytes.len(),
                got: 0,
            }),
            Reply::NoResponseRequired | Reply::WriteFailure => {
                panic!("无回包事务不能进入 read_exact")
            }
        }
    }

    fn set_timeout(&mut self, timeout: Duration) -> Result<(), FeetechError> {
        assert_eq!(timeout, Duration::from_millis(50));
        Ok(())
    }

    fn is_connected(&self) -> bool {
        true
    }
    fn discard_input(&mut self) -> Result<(), FeetechError> {
        self.pending_rx = None;
        Ok(())
    }
    fn drain_until_quiet(
        &mut self,
        quiet: Duration,
        budget: Duration,
    ) -> Result<DrainReport, FeetechError> {
        assert_eq!(quiet, Duration::from_millis(50));
        assert_eq!(budget, Duration::from_millis(150));
        self.clock.advance(DurationNs::from_millis(50));
        Ok(DrainReport {
            bytes: 0,
            deadline_hit: false,
        })
    }
}

/// 测试侧直接编码协议字面，不借生产编码器生成 expected TX。
fn packet(id: u8, instruction_or_status: u8, parameters: &[u8]) -> Vec<u8> {
    let mut frame = vec![
        0xff,
        0xff,
        id,
        u8::try_from(parameters.len() + 2).unwrap(),
        instruction_or_status,
    ];
    frame.extend_from_slice(parameters);
    let checksum = !frame[2..]
        .iter()
        .fold(0u8, |sum, byte| sum.wrapping_add(*byte));
    frame.push(checksum);
    frame
}

fn status(status: u8, data: &[u8]) -> Reply {
    Reply::Bytes(packet(MOTOR, status, data))
}

fn sync_read(address: u8, width: u8, reply: Reply) -> WireStep {
    WireStep {
        tx: packet(0xfe, 0x82, &[address, width, MOTOR]),
        reply,
    }
}

fn safety(reply: Reply) -> WireStep {
    WireStep {
        tx: packet(0xfe, 0x83, &[40, 1, MOTOR, 0]),
        reply,
    }
}

fn goal() -> WireStep {
    WireStep {
        tx: packet(0xfe, 0x83, &[42, 2, MOTOR, 0xdc, 0x05]),
        reply: Reply::NoResponseRequired,
    }
}

fn torque(value: u8, reply: Reply) -> WireStep {
    WireStep {
        tx: packet(MOTOR, 3, &[40, value]),
        reply,
    }
}

fn device_script() -> Vec<WireStep> {
    vec![
        // 真实 preflight：Safety、Connect 两子事务、Confirm。
        safety(Reply::NoResponseRequired),
        WireStep {
            tx: packet(MOTOR, 1, &[]),
            reply: status(0, &[]),
        },
        WireStep {
            tx: packet(MOTOR, 2, &[3, 2]),
            reply: status(0, &[0x09, 0x03]),
        },
        sync_read(40, 1, status(0, &[0])),
        // Enable(true)：目标回读先不匹配，再恢复；扭矩写先设备 NACK，再恢复。
        sync_read(56, 2, status(0, &[0xdc, 0x05])),
        goal(),
        sync_read(42, 2, status(0, &[0xff, 0x07])),
        goal(),
        sync_read(42, 2, status(0, &[0xdc, 0x05])),
        torque(1, status(0x24, &[])),
        torque(1, status(0, &[])),
        // 成功 normal 后同一真实 tick 继续周期采样。
        sync_read(56, 2, status(0, &[0xdc, 0x05])),
        sync_read(58, 2, status(0, &[0, 0])),
        // Disable 超时进入真实 Fault，Safety 第一次写失败，随后写和确认成功。
        torque(0, Reply::ReadTimeout),
        safety(Reply::WriteFailure),
        safety(Reply::NoResponseRequired),
        sync_read(40, 1, status(0, &[0])),
    ]
}

struct TickFixture {
    clock: Arc<MockClock>,
    ctrl: Arc<HardRtCtrlInner>,
    probe: Arc<SerialPreemptProbe>,
    diag: Arc<SerialDiagRing>,
    adapter: FeetechAdapter<ScriptedTransport>,
    tick: SerialTick,
    metrics: SerialIoThreadMetrics,
    writes: Arc<AtomicUsize>,
    expected_writes: usize,
}

impl TickFixture {
    fn new() -> Self {
        let clock = Arc::new(MockClock::new(MonoRawNs(INITIAL_NS)));
        let ctrl = Arc::new(HardRtCtrlInner::new_serial(
            crate::hard_rt_ctrl::test_authority(true),
        ));
        let probe = Arc::new(SerialPreemptProbe::new(clock.clone(), ctrl.clone()));
        let diag = Arc::new(SerialDiagRing::new());
        let writes = Arc::new(AtomicUsize::new(0));
        let script = device_script();
        let expected_writes = script.len();
        let transport = ScriptedTransport {
            steps: script.into(),
            pending_rx: None,
            clock: clock.clone(),
            writes: writes.clone(),
        };
        let bus = FeetechBus::new(
            transport,
            HashMap::from([("diagnostic_joint".to_owned(), Motor { id: MOTOR })]),
        )
        .unwrap();
        let adapter = FeetechAdapter::new(
            FeetechAdapterConfig {
                clock: clock.clone(),
                preempt: Some(probe.clone()),
                diag: Some(diag.clone()),
                call_deadline: Duration::from_millis(50),
                adapter_name: "独立诊断验收设备",
                joint_names: vec!["diagnostic_joint".to_owned()],
                calibrations: Vec::new(),
            },
            bus,
        )
        .unwrap();
        Self {
            tick: SerialTick::new(true, 1, clock.now()),
            clock,
            ctrl,
            probe,
            diag,
            adapter,
            metrics: SerialIoThreadMetrics::default(),
            writes,
            expected_writes,
        }
    }

    fn step(&mut self) {
        let deadline = MonoRawNs(self.clock.now().0 + 5_000_000);
        let context = SerialTickContext {
            ctrl: &self.ctrl,
            probe: &self.probe,
            diag: self.diag.as_ref(),
            clock: self.clock.as_ref(),
            metrics: &self.metrics,
        };
        self.tick.tick(
            &mut self.adapter,
            &JointCommandBuf::default().as_view(),
            &context,
            deadline,
        );
        self.clock.advance(DurationNs::from_millis(5));
    }

    fn normal(&mut self, enabled: bool, expected_response: AdapterResp) {
        let client = HardRtCtrlClient::new(self.ctrl.clone());
        let enable_owner = if enabled {
            let (owner, view) = self.ctrl.authority().reserve_enable().unwrap();
            // 断言失败不能丢失原 owner；仅原 seq 真完成后才按值移交 commit。
            let owner = std::mem::ManuallyDrop::new(owner);
            self.ctrl.start_slot().post(view).unwrap();
            Some(owner)
        } else {
            None
        };
        let NormalReqStatus::Submitted { seq } =
            client.request_normal(AdapterNormalReq::Enable(enabled))
        else {
            panic!("健康启动后必须接受 normal 请求")
        };
        self.step();
        assert_eq!(client.poll_last_normal_resp(seq), Some(expected_response));
        if let Some(owner) = enable_owner {
            match self
                .ctrl
                .authority()
                .commit_enable(std::mem::ManuallyDrop::into_inner(owner))
            {
                Ok(_) => {}
                Err(original) => {
                    let reason = original.reason;
                    let _held = std::mem::ManuallyDrop::new((original, self.ctrl.clone()));
                    panic!("真实 Enable 成功后必须能提交原 owner：{reason:?}");
                }
            }
        }
    }
}

fn exercise_real_serial_path() -> Vec<DiagEvent> {
    let mut fixture = TickFixture::new();
    for phase in [
        ConvergePhase::Connect,
        ConvergePhase::Confirm,
        ConvergePhase::Idle,
    ] {
        fixture.step();
        assert_eq!(fixture.tick.convergence().phase(), phase);
    }
    assert_eq!(
        fixture.tick.convergence().boot_outcome(),
        BootOutcome::Passed
    );
    assert!(fixture.ctrl.rx_trust.load(Ordering::Acquire));
    fixture.normal(true, AdapterResp::Ok);
    assert_eq!(fixture.ctrl.safety_state(), OutputSafetyState::TorqueOn);
    fixture.normal(
        false,
        AdapterResp::Err(AdapterErr::Comm(CommErrorKind::Timeout)),
    );
    assert_eq!(
        fixture.ctrl.mailbox_snapshot().unwrap().code,
        FaultCode::CommFault
    );
    for phase in [
        ConvergePhase::StopRetry,
        ConvergePhase::Confirm,
        ConvergePhase::AwaitAck,
    ] {
        fixture.step();
        assert_eq!(fixture.tick.convergence().phase(), phase);
    }
    assert_eq!(
        fixture.ctrl.safety_state(),
        OutputSafetyState::AdapterUncertain
    );
    assert_eq!(
        fixture.writes.load(Ordering::Relaxed),
        fixture.expected_writes,
        "设备脚本必须全部实际执行"
    );
    assert_eq!(
        fixture
            .metrics
            .normal_completed_count
            .load(Ordering::Relaxed),
        2
    );
    assert_eq!(fixture.metrics.tick_count.load(Ordering::Relaxed), 8);
    assert_eq!(fixture.diag.dropped(), 0);
    let mut events = Vec::new();
    while let Some(event) = fixture.diag.pop() {
        events.push(event);
    }
    assert!(!events.is_empty(), "真实设备诊断不能退化为空样本");
    assert!(
        events
            .iter()
            .all(|event| (INITIAL_NS..=fixture.clock.now().0).contains(&event.stamp_ns))
    );
    assert!(
        events
            .windows(2)
            .all(|pair| pair[0].stamp_ns <= pair[1].stamp_ns)
    );
    events
}

#[test]
fn serial_thread_emits_no_tracing() {
    let (events, counts) = in_serial_thread(exercise_real_serial_path);
    assert!(
        events
            .iter()
            .any(|event| event.site == 20 && event.kind == 7),
        "必须实际经过停止重试"
    );
    assert_eq!(
        counts.business_events.load(Ordering::Relaxed),
        0,
        "真实串口业务区不得发 tracing event"
    );
    assert_eq!(
        counts.business_spans.load(Ordering::Relaxed),
        0,
        "真实串口业务区不得创建 tracing span"
    );
}

#[test]
fn diag_retry_recovered_fields() {
    let (events, counts) = in_serial_thread(exercise_real_serial_path);
    assert_eq!(counts.business_events.load(Ordering::Relaxed), 0);
    let fields = |site| {
        events
            .iter()
            .filter(|event| event.site == site)
            .map(|event| (event.kind, event.a, event.b, event.seq_lo))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        fields(2),
        [(1, MOTOR, 1, 547), (2, 0, 2, 0)],
        "Goal Retry / Recovered 必须保留真实尝试次数与差值"
    );
    assert_eq!(
        fields(3),
        [
            (6, MOTOR, 63, 0), // Torque NACK 不冒充通信错误；设备状态另存。
            (5, MOTOR, 0x24, 0),
            (1, MOTOR, 1, 0),
            (2, MOTOR, 2, 0),
            (6, MOTOR, 0, 0), // 下一次 disable 的真实 Timeout。
            (0, 0, 0, 0),
        ]
    );
    assert_eq!(
        fields(7),
        [(0, 3, 0, 0)],
        "Safety 实际写失败必须保留 IoWrite 分类"
    );
    let stop_retries: Vec<_> = events
        .iter()
        .filter(|event| event.site == 20 && event.kind == 7)
        .map(|event| (event.a, event.b, event.seq_lo))
        .collect();
    assert_eq!(
        stop_retries,
        [(1, 3, 0)],
        "StopRetry 必须来自第一轮真实 Safety 写失败"
    );
}

#[test]
fn serial_subscriber_canary_detects_an_actual_business_event() {
    let ((), counts) = in_serial_thread(|| {
        tracing::warn!(target: "post_e_diag_business_negative", "反例：业务 tracing 必须被捕获");
        let span = tracing::info_span!("反例：业务 span 必须被捕获");
        let _entered = span.enter();
    });
    assert_eq!(counts.business_events.load(Ordering::Relaxed), 1);
    assert_eq!(counts.business_spans.load(Ordering::Relaxed), 1);
}
