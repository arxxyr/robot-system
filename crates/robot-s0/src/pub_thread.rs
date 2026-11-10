//! `s0_pub_thread` —— S0 四线程模型的"外发 iceoryx2 枢纽"。
//!
//! 对应 `docs/00_design.md` §1.2：
//! - 读 `rt_snap_rdr`（`RtSnapshotReader<RtSnapshot>`，`rt_thread` 每 tick 更新）
//!   + `state_rdr`（`RtSnapshotReader<S0StateSnapshot>`，`ipc_thread` 每 tick 更新）
//! - 按设定频率（默认 50 Hz）构造 `JointState` body → iceoryx2 publish
//! - 独立频率（默认 50 Hz）发 S0 自身 `Heartbeat`（`source_id = 1`，§3.5 约定）
//!
//! ### 线程语义
//!
//! - **非 RT**（普通优先级）。轮询周期 1 ms，每次 tick 检查两个发布边界是否到期。
//! - `Iceoryx2Publisher<T>` 同样 `!Send`（iceoryx2 `ipc::Service` 的
//!   `SingleThreaded` `arc_sync_policy`），入参用 `FnOnce` factory closure 在线程
//!   内构造，和 `ipc_thread` 对称。
//! - 退出信号走 `Arc<AtomicBool>`。
//!
//! ### 为什么 `pub_thread` 不直接读命令
//!
//! `pub_thread` 只发"观测"（state + 自身存活），不回读 command —— command 属于
//! `rt_thread` 的输入，发布它会在 `topic` 图上形成回声。监控可以订阅 `ipc_thread`
//! 上游的 command `topic`。
//!
//! ### `layout_id`
//!
//! `JointState.layout_id` = `cfg.joint_layout_id`——**生产权威值由
//! `s0_daemon::resolve()` 从 config 派生**（`compute_layout_id`，v0.5
//! 候选 E 段(3)），`PubThreadConfig` 仅承载该值；struct Default 的
//! `0xCAFE_BABE_DEAD_BEEF` 占位只服务 CLI-only 边界与单测（对齐
//! `hello_pubsub`）。**adapter 不是也不得成为第二权威源**。

#![forbid(unused_must_use)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use robot_core::adapter::RtSnapshot;
use robot_core::time::RobotClock;
use robot_msg::build_helpers::{build_heartbeat, build_joint_state_with_s0};
use robot_msg::markers;
use robot_msg::msg::{S0ControlLink, S0FaultCode, S0Mode, S0OutputMode, S0SafetyState};
use robot_transport::iceoryx2::Iceoryx2Publisher;
use robot_transport::{RtSnapshotReader, traits::Publisher};

use crate::io_thread::{CameraFrames, camera_frame_valid};
use crate::state_machine::S0StateSnapshot;
use robot_transport::error::TransportError;

// ============================================================================
// Config / Metrics / Handle
// ============================================================================

/// `pub_thread` 配置。
#[derive(Clone, Debug)]
pub struct PubThreadConfig {
    /// `JointState` 发布频率（Hz）。50 Hz 对齐 §1.2 + `dimos` 上游兼容。
    pub state_hz: f64,
    /// `Heartbeat`（S0 自身）发布频率（Hz）。50 Hz 默认。
    pub heartbeat_hz: f64,
    /// `Heartbeat.source_id`：S0 固定 1（§3.5 约定）。
    pub s0_source_id: u32,
    /// `JointState.layout_id`。生产权威值 = `s0_daemon::resolve()` config
    /// 派生（段(3)，与 ipc gate 同一 `ResolvedArgs.authoritative_layout`
    /// 单源）；Default 占位只服务 CLI-only 与单测。
    pub joint_layout_id: u64,
    /// 主循环 busy-poll 间隔（纳秒）。默认 1 ms，对应最细的"发布边界检查"精度。
    pub poll_interval_ns: u64,
}

impl Default for PubThreadConfig {
    fn default() -> Self {
        Self {
            state_hz: 50.0,
            heartbeat_hz: 50.0,
            s0_source_id: 1,
            joint_layout_id: 0xCAFE_BABE_DEAD_BEEF,
            poll_interval_ns: 1_000_000, // 1 ms
        }
    }
}

/// 非 RT 发布路径的观测计数器。
#[derive(Debug, Default)]
pub struct PubThreadMetrics {
    pub tick_count: AtomicU64,
    pub joint_state_published: AtomicU64,
    pub joint_state_errors: AtomicU64,
    pub heartbeat_published: AtomicU64,
    pub heartbeat_errors: AtomicU64,
    pub camera_published: AtomicU64,
    pub camera_errors: AtomicU64,
    pub camera_dropped: AtomicU64,
    /// **A2 freeze hook**（v0.3 / test-only，cfg-gated）：tick loop 顶端 spin
    /// 到该单调时钟 ns 为止；0 = 不冻。详 [`crate::freeze_hook`] 模块 doc。
    /// 仅 `cfg(any(test, feature = "test-freeze-hook"))` 编译时存在，**不进
    /// 生产 release**。
    #[cfg(any(test, feature = "test-freeze-hook"))]
    pub freeze_until_ns: AtomicU64,
}

/// 闭包在线程内构造的两个 iceoryx2 publisher。
pub struct IpcPublishers {
    pub joint_state: Iceoryx2Publisher<markers::JointState>,
    pub heartbeat: Iceoryx2Publisher<markers::Heartbeat>,
}

/// 在线程内按原顺序持有发布端口与 Node；兼容测试输入不创建第二 Node。
pub struct OwnedIpcPublishers {
    publishers: IpcPublishers,
    cameras: Vec<CameraPublisher>,
    _node: Option<iceoryx2::prelude::Node<iceoryx2::service::ipc::Service>>,
}
impl OwnedIpcPublishers {
    #[must_use]
    pub fn new(
        publishers: IpcPublishers,
        node: iceoryx2::prelude::Node<iceoryx2::service::ipc::Service>,
    ) -> Self {
        Self {
            publishers,
            cameras: Vec::new(),
            _node: Some(node),
        }
    }

    /// 每路相机与原 state/heartbeat 共用本线程和同一个 Node。
    #[must_use]
    pub fn with_camera(
        mut self,
        publisher: Iceoryx2Publisher<markers::ImageHeader>,
        frame_id: String,
        frames: Arc<CameraFrames>,
    ) -> Self {
        self.cameras.push(CameraPublisher {
            publisher,
            frame_id,
            frames,
            seq: 0,
        });
        self
    }
}
impl From<IpcPublishers> for OwnedIpcPublishers {
    fn from(publishers: IpcPublishers) -> Self {
        Self {
            publishers,
            cameras: Vec::new(),
            _node: None,
        }
    }
}

struct CameraPublisher {
    publisher: Iceoryx2Publisher<markers::ImageHeader>,
    frame_id: String,
    frames: Arc<CameraFrames>,
    seq: u64,
}

/// `pub_thread` 启动后返回的控制 handle。
pub struct PubThreadHandle {
    join: JoinHandle<()>,
    stop: Arc<AtomicBool>,
    metrics: Arc<PubThreadMetrics>,
    ready: crate::ipc_thread::ReadyReader<TransportError>,
}

impl PubThreadHandle {
    pub fn poll_ready(&mut self) -> crate::ipc_thread::ReadyState<'_, TransportError> {
        self.ready.poll()
    }
    pub fn request_stop(&self) {
        self.stop.swap(true, Ordering::AcqRel);
    }

    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.join.is_finished()
    }

    #[must_use]
    pub fn metrics(&self) -> &Arc<PubThreadMetrics> {
        &self.metrics
    }

    /// # Errors
    ///
    /// `JoinHandle::join` 的 `Err`（线程 panic）。
    pub fn join(self) -> thread::Result<()> {
        self.join.join()
    }
}

// ============================================================================
// 入口
// ============================================================================

/// 启动 `pub_thread`。
///
/// - `cfg`：发布频率 + `source_id` + `layout_id`（`resolve()` 派生权威值）。
/// - `clock`：`SystemClock` 生产 / `MockClock` 测试。
/// - `publishers_factory`：`FnOnce` 闭包，在 `pub_thread` 内部 open iceoryx2
///   publisher（对齐 `ipc_thread` 的 subscriber factory；同理 `!Send` 绕过）。
/// - `rt_snap_rdr`：`rt_thread` 发布的 joint state 快照（seqlock 读端）。
/// - `state_rdr`：`ipc_thread` 发布的 state machine 快照（seqlock 读端）。
///
/// # Errors
///
/// `std::io::Error` 当 `thread::Builder::spawn` 失败。
// 保留已批准的构造参数：独立预建 stop 与可选 Serial 诊断源不可隐式构造。
#[allow(clippy::too_many_arguments)]
pub fn spawn_pub_thread<F, P>(
    cfg: PubThreadConfig,
    clock: Arc<dyn RobotClock>,
    publishers_factory: F,
    rt_snap_rdr: RtSnapshotReader<RtSnapshot>,
    state_rdr: RtSnapshotReader<S0StateSnapshot>,
    rt_ctrl: Arc<crate::hard_rt_ctrl::HardRtCtrlInner>,
    diag: Option<Arc<crate::diag::SerialDiagRing>>,
    stop: Arc<AtomicBool>,
) -> std::io::Result<PubThreadHandle>
where
    F: FnOnce() -> P + Send + 'static,
    P: Into<OwnedIpcPublishers>,
{
    spawn_pub_thread_with_cameras(
        cfg,
        clock,
        move || Ok(publishers_factory()),
        rt_snap_rdr,
        state_rdr,
        rt_ctrl,
        diag,
        stop,
    )
}

/// 相机发布端也在线程 factory 内创建；其创建错误经原 Ready 通道拒绝启动。
#[allow(clippy::too_many_arguments)]
pub fn spawn_pub_thread_with_cameras<F, P>(
    cfg: PubThreadConfig,
    clock: Arc<dyn RobotClock>,
    publishers_factory: F,
    rt_snap_rdr: RtSnapshotReader<RtSnapshot>,
    state_rdr: RtSnapshotReader<S0StateSnapshot>,
    rt_ctrl: Arc<crate::hard_rt_ctrl::HardRtCtrlInner>,
    diag: Option<Arc<crate::diag::SerialDiagRing>>,
    stop: Arc<AtomicBool>,
) -> std::io::Result<PubThreadHandle>
where
    F: FnOnce() -> Result<P, TransportError> + Send + 'static,
    P: Into<OwnedIpcPublishers>,
{
    #[cfg(feature = "test-comm-fault-hook")]
    crate::comm_fault_hook::before_spawn("pub")?;
    let metrics = Arc::new(PubThreadMetrics::default());

    let join_stop = Arc::clone(&stop);
    let join_metrics = Arc::clone(&metrics);

    // 容量 1 的单次结果，OS spawn 成功后不等待工厂即可交还原句柄。
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);

    let join = thread::Builder::new()
        .name("s0_pub_thread".into())
        .spawn(move || {
            let mut pubs: OwnedIpcPublishers = match publishers_factory() {
                Ok(publishers) => publishers.into(),
                Err(error) => {
                    let _ = ready_tx.try_send(Err(error));
                    return;
                }
            };
            let _ = ready_tx.try_send(Ok(()));
            drop(ready_tx);
            run_loop(
                &cfg,
                clock.as_ref(),
                &mut pubs.publishers,
                &mut pubs.cameras,
                &rt_snap_rdr,
                &state_rdr,
                &rt_ctrl,
                diag.as_deref(),
                &join_stop,
                &join_metrics,
            );
        })?;

    Ok(PubThreadHandle {
        join,
        stop,
        metrics,
        ready: crate::ipc_thread::ReadyReader::new(ready_rx),
    })
}

// ============================================================================
// 主循环
// ============================================================================

#[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)]
fn run_loop(
    cfg: &PubThreadConfig,
    clock: &dyn RobotClock,
    pubs: &mut IpcPublishers,
    cameras: &mut [CameraPublisher],
    rt_snap_rdr: &RtSnapshotReader<RtSnapshot>,
    state_rdr: &RtSnapshotReader<S0StateSnapshot>,
    rt_ctrl: &crate::hard_rt_ctrl::HardRtCtrlInner,
    diag: Option<&crate::diag::SerialDiagRing>,
    stop: &AtomicBool,
    metrics: &PubThreadMetrics,
) {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let state_period_ns = (1e9_f64 / cfg.state_hz) as u64;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let hb_period_ns = (1e9_f64 / cfg.heartbeat_hz) as u64;

    let mut last_state_ns: u64 = clock.now().0;
    let mut last_hb_ns: u64 = clock.now().0;
    let mut state_seq: u64 = 0;
    let mut hb_seq: u64 = 0;
    let mut last_diag_summary_ns = clock.now().0;
    let mut next_camera = 0;

    while crate::worker_stop::worker_running(crate::worker_stop::WorkerKind::Pub, clock, stop) {
        // ─── A2 freeze hook（test-only，0 cost when freeze_until_ns == 0）───
        #[cfg(any(test, feature = "test-freeze-hook"))]
        crate::freeze_hook::freeze_check(&metrics.freeze_until_ns, clock, stop);

        let now = clock.now().0;
        drain_serial_diag(diag, rt_ctrl, now, &mut last_diag_summary_ns);

        // 1) JointState
        if now.saturating_sub(last_state_ns) >= state_period_ns {
            publish_joint_state(
                &mut pubs.joint_state,
                cfg,
                rt_snap_rdr,
                state_rdr,
                rt_ctrl,
                state_seq,
                now,
                metrics,
            );
            state_seq = state_seq.wrapping_add(1);
            last_state_ns = now;
        }

        // 2) Heartbeat
        if now.saturating_sub(last_hb_ns) >= hb_period_ns {
            publish_s0_heartbeat(&mut pubs.heartbeat, cfg, hb_seq, now, metrics);
            hb_seq = hb_seq.wrapping_add(1);
            last_hb_ns = now;
        }

        // 各路轮转，每轮最多一张图像；state/heartbeat 总是优先。
        if let Some(camera) = cameras.get_mut(next_camera) {
            publish_camera_frame(camera, cfg.s0_source_id, clock, metrics);
            next_camera = (next_camera + 1) % cameras.len();
        }

        metrics.tick_count.fetch_add(1, Ordering::Relaxed);
        thread::sleep(Duration::from_nanos(cfg.poll_interval_ns));
    }
}

fn publish_camera_frame(
    camera: &mut CameraPublisher,
    source_id: u32,
    clock: &dyn RobotClock,
    metrics: &PubThreadMetrics,
) {
    let Some(frame) = camera.frames.try_take() else {
        return;
    };
    if !camera_frame_valid(&frame, clock.now().0) || !camera.frames.is_active() {
        metrics.camera_dropped.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let body = build_camera_header(&frame, &camera.frame_id);
    let size = 1024 + body.len() + frame.data.len();
    let result = camera.publisher.loan(size).and_then(|mut sample| {
        sample.set_seq(camera.seq);
        sample.set_source_id(source_id);
        sample.set_stamp_ns(frame.stamp_ns);
        sample.set_body_bytes(&body);
        sample.set_blob_bytes(&frame.data);
        sample.publish()
    });
    match result {
        Ok(()) => {
            camera.seq = camera.seq.wrapping_add(1);
            metrics.camera_published.fetch_add(1, Ordering::Relaxed);
        }
        Err(_) => {
            metrics.camera_errors.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn build_camera_header(frame: &crate::camera::CameraFrame, frame_id: &str) -> Vec<u8> {
    use robot_msg::msg::{
        BlobDesc, BlobDescArgs, BlobLayout, Compression, Dtype, Endian, ImageHeader,
        ImageHeaderArgs,
    };
    let mut builder = flatbuffers::FlatBufferBuilder::with_capacity(256);
    let shape = builder.create_vector(&[frame.height, frame.width, 3]);
    let blob = BlobDesc::create(
        &mut builder,
        &BlobDescArgs {
            size: u64::try_from(frame.data.len()).expect("相机帧长度已受16MiB上限校验"),
            alignment: 1,
            dtype: Dtype::U8,
            shape: Some(shape),
            layout: BlobLayout::HWC,
            endian: Endian::Little,
            compression: Compression::None_,
            ..BlobDescArgs::default()
        },
    );
    let encoding = builder.create_string("bgr8");
    let order = builder.create_string("BGR");
    let frame_id = builder.create_string(frame_id);
    let header = ImageHeader::create(
        &mut builder,
        &ImageHeaderArgs {
            blob: Some(blob),
            width: frame.width,
            height: frame.height,
            encoding: Some(encoding),
            step: frame.step,
            color_order: Some(order),
            stamp_ns: frame.stamp_ns,
            frame_id: Some(frame_id),
        },
    );
    builder.finish(header, None);
    builder.finished_data().to_vec()
}

#[allow(clippy::too_many_arguments)]
fn publish_joint_state(
    pub_: &mut Iceoryx2Publisher<markers::JointState>,
    cfg: &PubThreadConfig,
    rt_snap_rdr: &RtSnapshotReader<RtSnapshot>,
    state_rdr: &RtSnapshotReader<S0StateSnapshot>,
    rt_ctrl: &crate::hard_rt_ctrl::HardRtCtrlInner,
    seq: u64,
    now_ns: u64,
    metrics: &PubThreadMetrics,
) {
    let snap = rt_snap_rdr.load();
    let n = snap.state.joint_count as usize;
    // 把 f32 数组切片 → f64 Vec（build_joint_state 要求 f64；Part 2 承担一次窄→宽）。
    let positions: Vec<f64> = snap.state.positions[..n]
        .iter()
        .map(|x| f64::from(*x))
        .collect();
    let velocities: Vec<f64> = snap.state.velocities[..n]
        .iter()
        .map(|x| f64::from(*x))
        .collect();
    let torques: Vec<f64> = snap.state.torques[..n]
        .iter()
        .map(|x| f64::from(*x))
        .collect();
    // task-30 v9 pre-2 + M7 阶段 5：从 S0StateSnapshot 抽 4 个状态字段；
    // S0SafetyState 直接从共享 `HardRtCtrlInner.safety_state` 原子读
    // （rt_thread / serial_io_thread 都把当前 OutputSafetyState 写到这里），
    // 转 wire enum（数值已 1:1 对齐，schema_compat 锁测保证）。
    let s0_snap = state_rdr.load();
    let (s0_mode, s0_control_link, s0_output_mode, s0_fault_code) = map_state_to_wire(&s0_snap);
    let s0_safety_state = map_safety_state_to_wire(rt_ctrl.safety_state().to_u8());
    let body = build_joint_state_with_s0(
        cfg.joint_layout_id,
        snap.state.stamp_ns,
        &positions,
        &velocities,
        &torques,
        "",
        s0_mode,
        s0_control_link,
        s0_output_mode,
        s0_safety_state,
        s0_fault_code,
    );
    let upper = 1024 + body.len();
    match pub_.loan(upper) {
        Ok(mut w) => {
            w.set_seq(seq);
            w.set_source_id(cfg.s0_source_id);
            w.set_stamp_ns(now_ns);
            w.set_body_bytes(&body);
            match w.publish() {
                Ok(()) => {
                    metrics
                        .joint_state_published
                        .fetch_add(1, Ordering::Relaxed);
                }
                Err(_) => {
                    metrics.joint_state_errors.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        Err(_) => {
            metrics.joint_state_errors.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// task-30 v9 pre-2：把 [`S0StateSnapshot`] 映射成 4 个 wire enum
/// (`S0Mode` / `S0ControlLink` / `S0OutputMode` / `S0FaultCode`)。
/// `S0SafetyState` 由 M3 接 `OutputSafetyProbe` 后再填，本函数不返回。
fn map_state_to_wire(snap: &S0StateSnapshot) -> (S0Mode, S0ControlLink, S0OutputMode, S0FaultCode) {
    use crate::state_machine::{ControlLink, FaultCode, OutputMode, S0State};

    let (mode, link, out, fc) = match snap.state {
        S0State::Safe => (
            S0Mode::Safe,
            S0ControlLink::Unknown,
            S0OutputMode::Unknown,
            S0FaultCode::Unknown,
        ),
        S0State::Ready => (
            S0Mode::Ready,
            S0ControlLink::Unknown,
            S0OutputMode::Unknown,
            S0FaultCode::Unknown,
        ),
        S0State::Enabled {
            control_link,
            output_mode,
            ..
        } => {
            let link_w = match control_link {
                ControlLink::Ok => S0ControlLink::Ok,
                ControlLink::Lost => S0ControlLink::Lost,
            };
            let out_w = match output_mode {
                OutputMode::Active => S0OutputMode::Active,
                OutputMode::Hold => S0OutputMode::Hold,
                OutputMode::Zero => S0OutputMode::Zero,
                OutputMode::Stop => S0OutputMode::Stop,
            };
            (S0Mode::Enabled, link_w, out_w, S0FaultCode::Unknown)
        }
        S0State::Fault(fc) => {
            let fc_w = match fc {
                FaultCode::AdapterErr => S0FaultCode::AdapterErr,
                FaultCode::CmdDeadlineExpired => S0FaultCode::CmdDeadlineExpired,
                FaultCode::LimitViolation => S0FaultCode::LimitViolation,
                FaultCode::WatchdogTimeout => S0FaultCode::WatchdogTimeout,
                FaultCode::RtOverrun => S0FaultCode::RtOverrun,
                FaultCode::SafetyStopFailed => S0FaultCode::SafetyStopFailed,
                FaultCode::CommFault => S0FaultCode::CommFault,
            };
            (
                S0Mode::Fault,
                S0ControlLink::Unknown,
                S0OutputMode::Unknown,
                fc_w,
            )
        }
        S0State::Stopping => (
            S0Mode::Stopping,
            S0ControlLink::Unknown,
            S0OutputMode::Unknown,
            S0FaultCode::Unknown,
        ),
    };
    (mode, link, out, fc)
}

/// task-30 v9 / M7 阶段 5：把共享 `HardRtCtrlInner.safety_state` 的原子
/// `u8` 转 wire `S0SafetyState`。**数值 1:1 对齐**（详
/// `crates/robot-msg/tests/compat.rs` 的
/// `s0_safety_state_wire_matches_core_output_safety_state` 锁测，schema 与
/// core 不再独立演进）。未知值（理论上不该出现，atomic 写者只 store
/// `OutputSafetyState::to_u8`）由 [`OutputSafetyState::from_u8`] 兜底成
/// `TorqueUnknown`，let wire 端不被假绿带跑。
fn map_safety_state_to_wire(raw: u8) -> S0SafetyState {
    use robot_core::executor_ctrl::OutputSafetyState;
    match OutputSafetyState::from_u8(raw) {
        OutputSafetyState::TorqueUnknown => S0SafetyState::TorqueUnknown,
        OutputSafetyState::TorqueOn => S0SafetyState::TorqueOn,
        OutputSafetyState::StopCommanded => S0SafetyState::StopCommanded,
        OutputSafetyState::TorqueOffConfirmed => S0SafetyState::TorqueOffConfirmed,
        OutputSafetyState::StopFailed => S0SafetyState::StopFailed,
        OutputSafetyState::AdapterUncertain => S0SafetyState::AdapterUncertain,
    }
}

fn publish_s0_heartbeat(
    pub_: &mut Iceoryx2Publisher<markers::Heartbeat>,
    cfg: &PubThreadConfig,
    seq: u64,
    now_ns: u64,
    metrics: &PubThreadMetrics,
) {
    let body = build_heartbeat(cfg.s0_source_id, seq, cfg.heartbeat_hz, now_ns);
    let upper = 256 + body.len();
    match pub_.loan(upper) {
        Ok(mut w) => {
            w.set_seq(seq);
            w.set_source_id(cfg.s0_source_id);
            w.set_stamp_ns(now_ns);
            w.set_body_bytes(&body);
            match w.publish() {
                Ok(()) => {
                    metrics.heartbeat_published.fetch_add(1, Ordering::Relaxed);
                }
                Err(_) => {
                    metrics.heartbeat_errors.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        Err(_) => {
            metrics.heartbeat_errors.fetch_add(1, Ordering::Relaxed);
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

/// 每 poll 有界消费，持续生产也不会阻塞状态与心跳发布。
fn drain_serial_diag(
    diag: Option<&crate::diag::SerialDiagRing>,
    ctrl: &crate::hard_rt_ctrl::HardRtCtrlInner,
    now: u64,
    last_summary: &mut u64,
) {
    let Some(diag) = diag else { return };
    for _ in 0..256 {
        let Some(event) = diag.pop() else { break };
        if matches!(event.site, 31 | 32) && matches!(event.kind, 0 | 1) {
            // Feetech 同连接零速/周期状态原字；不是故障或安全状态提交。
            tracing::debug!(
                target: "s0_wheel_observation",
                site = event.site,
                kind = event.kind,
                a = event.a,
                b = event.b,
                seq = event.seq_lo,
                stamp = event.stamp_ns,
                "串口设备观测",
            );
        } else {
            tracing::warn!(
                site = event.site,
                kind = event.kind,
                a = event.a,
                b = event.b,
                seq = event.seq_lo,
                stamp = event.stamp_ns,
                "串口诊断事件",
            );
        }
    }
    if now.saturating_sub(*last_summary) >= 1_000_000_000 {
        *last_summary = now;
        tracing::info!(
            dropped = diag.dropped(),
            safety = ?ctrl.safety_state(),
            comm_level = ctrl.comm_level.load(Ordering::Acquire),
            rx_trust = ctrl.rx_trust.load(Ordering::Acquire),
            inhibited = ctrl.inhibit_bits(),
            liveness = ctrl.liveness_counter.load(Ordering::Relaxed),
            "串口一秒摘要",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iceoryx2::prelude::NodeBuilder;
    use iceoryx2::service::ipc::Service as IxService;
    use robot_core::adapter::{JointStateBuf, MAX_JOINTS};
    use robot_core::time::{MockClock, MonoRawNs, SystemClock};
    use robot_msg::msg::JointState as FbJointState;
    use robot_transport::iceoryx2::{
        Iceoryx2Subscriber, TopicClass, publisher as make_pub, subscriber as make_sub,
    };
    use robot_transport::{LatestSubscriber, RtSnapshotPublisher};
    use std::sync::atomic::AtomicU32;

    static TEST_SEQ: AtomicU32 = AtomicU32::new(0);

    /// 真工厂可以阻塞或展开；父进程持有并有界回收测试子进程。
    fn supervise_factory_test(name: &str) -> bool {
        const KEY: &str = "ROBOT_PUB_OWNED_FACTORY_CHILD";
        if std::env::var(KEY).as_deref() == Ok(name) {
            let main = std::thread::current().id();
            std::panic::set_hook(Box::new(move |info| {
                eprintln!("publisher factory test: {info}");
                if std::thread::current().id() == main {
                    std::process::exit(101);
                }
            }));
            return false;
        }
        let directory = tempfile::tempdir().unwrap();
        let log_path = directory.path().join("child.log");
        let log = std::fs::File::create(&log_path).unwrap();
        let exact = format!("pub_thread::tests::{name}");
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &exact, "--nocapture"])
            .env(KEY, name)
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        let deadline = SystemClock.now().0 + 15_000_000_000;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if SystemClock.now().0 < deadline => {
                    thread::sleep(Duration::from_millis(2));
                }
                other => {
                    let _ = child.kill();
                    let reap_deadline = SystemClock.now().0 + 2_000_000_000;
                    while !matches!(child.try_wait(), Ok(Some(_))) {
                        if SystemClock.now().0 >= reap_deadline {
                            std::process::abort();
                        }
                        thread::sleep(Duration::from_millis(2));
                    }
                    panic!("工厂测试已 kill/reap：{other:?}");
                }
            }
        };
        let raw = std::fs::read_to_string(log_path).unwrap();
        assert!(status.success(), "{raw}");
        assert_eq!(
            raw.lines()
                .filter(|line| *line == "pub_factory_owned_verified")
                .count(),
            1,
            "{raw}"
        );
        true
    }

    fn exercise_owned_factory(fail_after_first_publisher: bool) {
        use crate::ipc_thread::ReadyState;
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let topics = fresh_topics();
        let (_rt_pub, rt_reader) = RtSnapshotPublisher::new(RtSnapshot::default());
        let (_state_pub, state_reader) = RtSnapshotPublisher::new(S0StateSnapshot::default());
        let authority = robot_core::control_authority::ControlAuthority::new(
            robot_core::control_authority::AuthorityInit::new(
                false,
                robot_core::executor_ctrl::OutputSafetyState::TorqueUnknown,
                robot_core::control_authority::InhibitSet::EMPTY,
            ),
        )
        .unwrap();
        let ctrl = Arc::new(crate::hard_rt_ctrl::HardRtCtrlInner::new(Arc::new(
            authority,
        )));
        let mut handle = spawn_pub_thread(
            PubThreadConfig::default(),
            Arc::new(SystemClock),
            move || {
                let node = NodeBuilder::new().create::<IxService>().unwrap();
                let joint_state =
                    make_pub::<markers::JointState>(&node, &topics.state, TopicClass::S0State)
                        .unwrap();
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                assert!(
                    !fail_after_first_publisher,
                    "真实 partial publisher 工厂展开"
                );
                let heartbeat =
                    make_pub::<markers::Heartbeat>(&node, &topics.heartbeat, TopicClass::Heartbeat)
                        .unwrap();
                OwnedIpcPublishers::new(
                    IpcPublishers {
                        joint_state,
                        heartbeat,
                    },
                    node,
                )
            },
            rt_reader,
            state_reader,
            ctrl,
            None,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(handle.poll_ready(), ReadyState::Pending));
        assert!(!handle.is_finished());
        handle.request_stop();
        release_tx.send(()).unwrap();
        let deadline = SystemClock.now().0 + 2_000_000_000;
        while !handle.is_finished() {
            assert!(SystemClock.now().0 < deadline);
            thread::sleep(Duration::from_millis(2));
        }
        let ready = matches!(handle.poll_ready(), ReadyState::Ready);
        let disconnected = matches!(handle.poll_ready(), ReadyState::Disconnected);
        let joined = handle.join();
        assert_eq!(ready, !fail_after_first_publisher);
        assert_eq!(disconnected, fail_after_first_publisher);
        assert_eq!(joined.is_err(), fail_after_first_publisher);
        // 原 panic payload 保至子进程退出，不以 Err 丢弃冒称正常回收。
        let _joined = std::mem::ManuallyDrop::new(joined);
        eprintln!("pub_factory_owned_verified");
        std::process::exit(0);
    }

    #[test]
    fn factory_blocked_returns_owned_handle_before_ready() {
        if supervise_factory_test("factory_blocked_returns_owned_handle_before_ready") {
            return;
        }
        exercise_owned_factory(false);
    }

    #[test]
    fn partial_factory_panic_returns_disconnected_and_original_join_error() {
        if supervise_factory_test(
            "partial_factory_panic_returns_disconnected_and_original_join_error",
        ) {
            return;
        }
        exercise_owned_factory(true);
    }

    struct Topics {
        state: String,
        heartbeat: String,
        camera: String,
    }

    fn fresh_topics() -> Topics {
        let id = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let base = format!("/test_pub_{}_{}", std::process::id(), id);
        Topics {
            state: format!("{base}/state"),
            heartbeat: format!("{base}/hb"),
            camera: format!("{base}/camera"),
        }
    }

    struct Observers {
        joint_state: Iceoryx2Subscriber<markers::JointState>,
        heartbeat: Iceoryx2Subscriber<markers::Heartbeat>,
        camera: Option<Iceoryx2Subscriber<markers::ImageHeader>>,
        _node: iceoryx2::prelude::Node<IxService>,
    }

    #[allow(clippy::type_complexity)]
    fn bring_up() -> (
        PubThreadHandle,
        RtSnapshotPublisher<RtSnapshot>,
        RtSnapshotPublisher<S0StateSnapshot>,
        Observers,
        Arc<crate::hard_rt_ctrl::HardRtCtrlInner>,
    ) {
        bring_up_with_camera_and_diag(None, None)
    }

    #[allow(clippy::type_complexity)]
    fn bring_up_with_camera_and_diag(
        camera: Option<Arc<CameraFrames>>,
        diag: Option<Arc<crate::diag::SerialDiagRing>>,
    ) -> (
        PubThreadHandle,
        RtSnapshotPublisher<RtSnapshot>,
        RtSnapshotPublisher<S0StateSnapshot>,
        Observers,
        Arc<crate::hard_rt_ctrl::HardRtCtrlInner>,
    ) {
        let topics = fresh_topics();
        // subscriber 端 Node 放主线程（测试观察者）。
        let sub_node = NodeBuilder::new().create::<IxService>().unwrap();
        let js_sub =
            make_sub::<markers::JointState>(&sub_node, &topics.state, TopicClass::S0State).unwrap();
        let hb_sub =
            make_sub::<markers::Heartbeat>(&sub_node, &topics.heartbeat, TopicClass::Heartbeat)
                .unwrap();
        let camera_sub = camera.as_ref().map(|_| {
            make_sub::<markers::ImageHeader>(&sub_node, &topics.camera, TopicClass::Sensor).unwrap()
        });

        let (rt_pub, rt_rdr) = RtSnapshotPublisher::<RtSnapshot>::new(RtSnapshot::default());
        let (state_pub, state_rdr) =
            RtSnapshotPublisher::<S0StateSnapshot>::new(S0StateSnapshot::default());

        let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
        let cfg = PubThreadConfig {
            state_hz: 200.0, // 加速测试
            heartbeat_hz: 200.0,
            ..PubThreadConfig::default()
        };

        let state_topic = topics.state.clone();
        let hb_topic = topics.heartbeat.clone();
        let factory = move || {
            let pub_node = NodeBuilder::new().create::<IxService>().unwrap();
            let joint_state =
                make_pub::<markers::JointState>(&pub_node, &state_topic, TopicClass::S0State)
                    .unwrap();
            let heartbeat =
                make_pub::<markers::Heartbeat>(&pub_node, &hb_topic, TopicClass::Heartbeat)
                    .unwrap();
            let image_pub = camera.as_ref().map(|_| {
                make_pub::<markers::ImageHeader>(&pub_node, &topics.camera, TopicClass::Sensor)
                    .unwrap()
            });
            let owned = OwnedIpcPublishers::new(
                IpcPublishers {
                    joint_state,
                    heartbeat,
                },
                pub_node,
            );
            match (image_pub, camera) {
                (Some(image_pub), Some(frames)) => {
                    owned.with_camera(image_pub, "test_camera".into(), frames)
                }
                _ => owned,
            }
        };

        // task-30 v9 / M7 阶段 5：默认共享 ctrl，safety_state = TorqueUnknown。
        // wire 读取同一权限原子的投影，不再直接 store 第二安全状态。
        let rt_ctrl = Arc::new(crate::hard_rt_ctrl::HardRtCtrlInner::new(Arc::new(
            robot_core::control_authority::ControlAuthority::new(
                robot_core::control_authority::AuthorityInit::new(
                    false,
                    robot_core::executor_ctrl::OutputSafetyState::TorqueUnknown,
                    robot_core::control_authority::InhibitSet::EMPTY,
                ),
            )
            .unwrap(),
        )));
        let handle = spawn_pub_thread(
            cfg,
            clock,
            factory,
            rt_rdr,
            state_rdr.clone(),
            Arc::clone(&rt_ctrl),
            diag,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .unwrap();
        (
            handle,
            rt_pub,
            state_pub,
            Observers {
                joint_state: js_sub,
                heartbeat: hb_sub,
                camera: camera_sub,
                _node: sub_node,
            },
            rt_ctrl,
        )
    }

    /// 轮询 `latest()` 直到拿到 `sample` 或超时 panic。
    ///
    /// 为什么不分两步（先 `wait_until(latest.is_some())` 再 `.latest()`）：iceoryx2
    /// 的 `latest()` 是 **drain 语义**（`iceoryx2_latest` 把 buffer 里所有样本消费
    /// 至只剩一条），第一次成功 drain 之后，紧接的第二次 `latest()` 在新 `sample`
    /// 到达前会返回 None。所以必须一次性"拿到就用"。
    fn drain_latest<T: robot_msg::markers::BodyMarker>(
        sub: &mut Iceoryx2Subscriber<T>,
        max: Duration,
    ) -> robot_transport::SampleLease<T> {
        let start = std::time::Instant::now();
        while start.elapsed() < max {
            if let Ok(Some(lease)) = sub.latest() {
                return lease;
            }
            thread::sleep(Duration::from_millis(2));
        }
        panic!("drain_latest timed out after {max:?}");
    }

    mod serial_diagnostics {
        use super::*;
        use std::collections::BTreeMap;
        use std::sync::Mutex;

        use robot_core::diag::{DiagEvent, DiagSink};
        use tracing::field::{Field, Visit};
        use tracing::span::{Attributes, Id, Record};
        use tracing::{Event, Level, Metadata, Subscriber};

        #[derive(Debug)]
        struct CapturedDiagnostic {
            target: &'static str,
            level: Level,
            fields: BTreeMap<&'static str, u64>,
        }

        impl Visit for CapturedDiagnostic {
            fn record_u64(&mut self, field: &Field, value: u64) {
                self.fields.insert(field.name(), value);
            }
            fn record_debug(&mut self, _field: &Field, _value: &dyn std::fmt::Debug) {}
        }

        struct DiagnosticSubscriber(Arc<Mutex<Vec<CapturedDiagnostic>>>);

        impl Subscriber for DiagnosticSubscriber {
            fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
                true
            }
            fn new_span(&self, _span: &Attributes<'_>) -> Id {
                Id::from_u64(1)
            }
            fn record(&self, _span: &Id, _values: &Record<'_>) {}
            fn record_follows_from(&self, _span: &Id, _follows: &Id) {}
            fn enter(&self, _span: &Id) {}
            fn exit(&self, _span: &Id) {}

            fn event(&self, event: &Event<'_>) {
                let mut captured = CapturedDiagnostic {
                    target: event.metadata().target(),
                    level: *event.metadata().level(),
                    fields: BTreeMap::new(),
                };
                event.record(&mut captured);
                self.0.lock().unwrap().push(captured);
            }
        }

        fn raw_group(stamp_ns: u64) -> [DiagEvent; 4] {
            // GV/PV 保留符号幅度原字：负零、负50，以及完整16位边界，不在 Pub 解码。
            [
                (0, 3, 0, 0),
                (1, 9, 1, 0x8032_0000),
                (1, 7, 0, 0x0000_8000),
                (1, 8, 1, u32::MAX),
            ]
            .map(|(kind, a, b, seq_lo)| DiagEvent {
                stamp_ns,
                site: 31,
                kind,
                a,
                b,
                seq_lo,
            })
        }

        #[test]
        fn drain_preserves_raw_words_order_and_only_known_observations_are_debug() {
            let diag = crate::diag::SerialDiagRing::new();
            let ctrl = crate::hard_rt_ctrl::HardRtCtrlInner::new(
                crate::hard_rt_ctrl::test_authority(true),
            );
            let before = ctrl.authority().snapshot();
            let mut events = raw_group(100).to_vec();
            events.extend(raw_group(100).map(|event| DiagEvent { site: 32, ..event }));
            events.extend([
                DiagEvent {
                    stamp_ns: 101,
                    site: 20,
                    kind: 2,
                    a: 3,
                    b: 4,
                    seq_lo: 5,
                },
                DiagEvent {
                    stamp_ns: 102,
                    site: 31,
                    kind: 2,
                    a: 6,
                    b: 7,
                    seq_lo: 8,
                },
            ]);
            for &event in &events {
                diag.record(event);
            }
            let captured = Arc::new(Mutex::new(Vec::new()));
            let mut last_summary = 200;
            tracing::subscriber::with_default(DiagnosticSubscriber(Arc::clone(&captured)), || {
                drain_serial_diag(Some(&diag), &ctrl, 200, &mut last_summary);
            });
            let captured = captured.lock().unwrap();
            assert_eq!(captured.len(), events.len());
            for (index, (actual, expected)) in captured.iter().zip(&events).enumerate() {
                let (level, target) = if index < 8 {
                    (Level::DEBUG, "s0_wheel_observation")
                } else {
                    (Level::WARN, "robot_s0::pub_thread")
                };
                assert_eq!(actual.target, target);
                assert_eq!(actual.level, level);
                assert_eq!(
                    actual.fields,
                    BTreeMap::from([
                        ("site", u64::from(expected.site)),
                        ("kind", u64::from(expected.kind)),
                        ("a", u64::from(expected.a)),
                        ("b", u64::from(expected.b)),
                        ("seq", u64::from(expected.seq_lo)),
                        ("stamp", expected.stamp_ns),
                    ])
                );
            }
            assert!(diag.pop().is_none());
            assert_eq!(diag.dropped(), 0);
            assert_eq!(ctrl.authority().snapshot(), before);
            assert_eq!(ctrl.comm_level.load(Ordering::Acquire), 0);
            assert!(ctrl.mailbox_snapshot().is_none());
        }

        #[test]
        fn finite_50hz_raw_batches_keep_state_and_heartbeat_publishing() {
            let diag = Arc::new(crate::diag::SerialDiagRing::new());
            let (handle, _rt, _state, mut observers, ctrl) =
                bring_up_with_camera_and_diag(None, Some(Arc::clone(&diag)));
            // 即使订阅或断言失败，也先停并真实 join 原 Pub worker。
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut state_seq =
                    drain_latest(&mut observers.joint_state, Duration::from_secs(2))
                        .view()
                        .unwrap()
                        .header
                        .seq();
                let mut heartbeat_seq =
                    drain_latest(&mut observers.heartbeat, Duration::from_secs(2))
                        .view()
                        .unwrap()
                        .header
                        .seq();
                let started = SystemClock.now().0;
                // 50 Hz 节拍下投十组；通过取决于真实发布推进，不把调度速度当性能门槛。
                for batch in 0..10_u64 {
                    SystemClock.sleep_until(robot_core::time::Deadline(MonoRawNs(
                        started + batch * 20_000_000,
                    )));
                    for event in raw_group(SystemClock.now().0) {
                        diag.record(event);
                    }
                    let queued_at = SystemClock.now().0;
                    let before_state = state_seq;
                    let before_heartbeat = heartbeat_seq;
                    let mut state_after_drain = false;
                    let mut heartbeat_after_drain = false;
                    assert!(
                        crate::test_util::wait_until(Duration::from_secs(2), || {
                            if let Some(lease) = observers.joint_state.latest().unwrap() {
                                let view = lease.view().unwrap();
                                state_seq = view.header.seq();
                                state_after_drain |=
                                    state_seq > before_state && view.header.stamp_ns() > queued_at;
                            }
                            if let Some(lease) = observers.heartbeat.latest().unwrap() {
                                let view = lease.view().unwrap();
                                heartbeat_seq = view.header.seq();
                                heartbeat_after_drain |= heartbeat_seq > before_heartbeat
                                    && view.header.stamp_ns() > queued_at;
                            }
                            state_after_drain && heartbeat_after_drain
                        }),
                        "第{batch}组诊断后状态与心跳必须继续发布"
                    );
                    // Pub 每轮先 drain 再发布；晚于入队的真实帧确认该组已消费。
                    assert!(diag.pop().is_none());
                }
                assert_eq!(diag.dropped(), 0);
                assert_eq!(ctrl.comm_level.load(Ordering::Acquire), 0);
                assert!(ctrl.mailbox_snapshot().is_none());
            }));
            handle.request_stop();
            handle.join().unwrap();
            if let Err(panic) = result {
                std::panic::resume_unwind(panic);
            }
        }
    }

    fn camera_test_frame(stamp_ns: u64) -> crate::camera::CameraFrame {
        crate::camera::CameraFrame {
            width: 2,
            height: 1,
            step: 6,
            stamp_ns,
            data: vec![1, 2, 3, 4, 5, 6],
        }
    }

    #[test]
    fn camera_frame_reaches_real_sensor_pubsub_with_original_stamp_and_bgr_blob() {
        let frames = Arc::new(CameraFrames::default());
        let (handle, _rt, _state, mut observers, _ctrl) =
            bring_up_with_camera_and_diag(Some(Arc::clone(&frames)), None);
        let stamp = SystemClock.now().0;
        frames.replace(camera_test_frame(stamp));
        let lease = drain_latest(observers.camera.as_mut().unwrap(), Duration::from_secs(2));
        let view = lease.view().unwrap();
        let image: robot_msg::msg::ImageHeader = view.body.get();
        assert_eq!(view.header.stamp_ns(), stamp);
        assert_eq!(image.stamp_ns(), stamp);
        assert_eq!((image.width(), image.height(), image.step()), (2, 1, 6));
        assert_eq!(image.encoding(), Some("bgr8"));
        assert_eq!(image.color_order(), Some("BGR"));
        assert_eq!(image.frame_id(), Some("test_camera"));
        assert_eq!(
            image
                .blob()
                .unwrap()
                .shape()
                .unwrap()
                .iter()
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert_eq!(view.blob.unwrap().bytes(), [1, 2, 3, 4, 5, 6]);
        assert_eq!(handle.metrics().camera_published.load(Ordering::Relaxed), 1);
        handle.request_stop();
        handle.join().unwrap();
    }

    #[test]
    fn camera_stale_or_invalidated_frame_is_not_published() {
        let topic = fresh_topics().camera;
        let node = NodeBuilder::new().create::<IxService>().unwrap();
        let mut subscriber =
            make_sub::<markers::ImageHeader>(&node, &topic, TopicClass::Sensor).unwrap();
        let publisher =
            make_pub::<markers::ImageHeader>(&node, &topic, TopicClass::Sensor).unwrap();
        let frames = Arc::new(CameraFrames::default());
        let mut camera = CameraPublisher {
            publisher,
            frame_id: "test_camera".into(),
            frames: Arc::clone(&frames),
            seq: 0,
        };
        let metrics = PubThreadMetrics::default();
        let clock = MockClock::new(MonoRawNs(crate::io_thread::CAMERA_MAX_AGE_NS + 1));
        frames.replace(camera_test_frame(0));
        publish_camera_frame(&mut camera, 1, &clock, &metrics);
        assert_eq!(metrics.camera_dropped.load(Ordering::Relaxed), 1);
        frames.replace(camera_test_frame(100));
        frames.clear();
        clock.set(MonoRawNs(100));
        publish_camera_frame(&mut camera, 1, &clock, &metrics);
        assert_eq!(metrics.camera_published.load(Ordering::Relaxed), 0);
        assert!(subscriber.latest().unwrap().is_none());
    }

    struct FrameTakenClock {
        frames: Arc<CameraFrames>,
        clock: MockClock,
    }

    impl RobotClock for FrameTakenClock {
        fn now(&self) -> MonoRawNs {
            assert!(
                self.frames.try_take().is_none(),
                "图像验证须先取帧再读取当前时钟"
            );
            self.clock.now()
        }
        fn sleep_until(&self, deadline: robot_core::time::Deadline) {
            self.clock.sleep_until(deadline);
        }
        fn sleep_for(&self, duration: robot_core::time::DurationNs) {
            self.clock.sleep_for(duration);
        }
    }

    #[test]
    fn camera_publish_uses_clock_after_take_when_frame_arrives_after_state_tick_time() {
        let topic = fresh_topics().camera;
        let node = NodeBuilder::new().create::<IxService>().unwrap();
        let mut subscriber =
            make_sub::<markers::ImageHeader>(&node, &topic, TopicClass::Sensor).unwrap();
        assert!(subscriber.latest().unwrap().is_none());
        let publisher =
            make_pub::<markers::ImageHeader>(&node, &topic, TopicClass::Sensor).unwrap();
        publisher.update_connections().unwrap();
        let frames = Arc::new(CameraFrames::default());
        let clock = FrameTakenClock {
            frames: Arc::clone(&frames),
            clock: MockClock::new(MonoRawNs(100)),
        };
        let state_tick_ns = clock.now().0;
        clock.clock.set(MonoRawNs(101));
        let frame = camera_test_frame(101);
        assert!(!camera_frame_valid(&frame, state_tick_ns));
        frames.replace(frame);
        let mut camera = CameraPublisher {
            publisher,
            frame_id: "test_camera".into(),
            frames,
            seq: 0,
        };
        let metrics = PubThreadMetrics::default();
        publish_camera_frame(&mut camera, 1, &clock, &metrics);
        let lease = drain_latest(&mut subscriber, Duration::from_secs(2));
        let view = lease.view().unwrap();
        let image: robot_msg::msg::ImageHeader = view.body.get();
        assert_eq!(image.stamp_ns(), 101);
        assert_eq!(view.header.stamp_ns(), 101);
        assert_eq!(metrics.camera_published.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.camera_dropped.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn camera_publisher_factory_error_rejects_ready_without_panicking() {
        use crate::ipc_thread::ReadyState;
        let (original, _rt, _state, _observers, ctrl) = bring_up();
        let (_, rt_reader) = RtSnapshotPublisher::new(RtSnapshot::default());
        let (_, state_reader) = RtSnapshotPublisher::new(S0StateSnapshot::default());
        let mut handle = spawn_pub_thread_with_cameras(
            PubThreadConfig::default(),
            Arc::new(SystemClock),
            || Err::<IpcPublishers, _>(TransportError::Backend("模拟相机publisher创建失败".into())),
            rt_reader,
            state_reader,
            ctrl,
            None,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        assert!(crate::test_util::wait_until(Duration::from_secs(2), || {
            handle.is_finished()
        }));
        assert!(
            matches!(handle.poll_ready(), ReadyState::Failed(TransportError::Backend(message)) if message == "模拟相机publisher创建失败")
        );
        handle.join().unwrap();
        original.request_stop();
        original.join().unwrap();
    }

    struct BlockingCamera {
        entered: Option<std::sync::mpsc::SyncSender<()>>,
        release: std::sync::mpsc::Receiver<()>,
        stopped: Arc<AtomicBool>,
    }

    impl robot_core::adapter::SensorAdapter for BlockingCamera {
        type Frame = crate::camera::CameraFrame;
        fn start(&mut self) -> Result<(), robot_core::error::RobotError> {
            Ok(())
        }
        fn poll_frame(
            &mut self,
            _deadline: robot_core::time::Deadline,
        ) -> Result<Option<Self::Frame>, robot_core::error::RobotError> {
            if let Some(entered) = self.entered.take() {
                entered.send(()).unwrap();
                // 仅故障隔离夹具故意阻塞；生产 adapter 始终非阻塞。
                thread::sleep(Duration::from_millis(100));
                self.release.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            Ok(None)
        }
        fn stop(&mut self) -> Result<(), robot_core::error::RobotError> {
            self.stopped.store(true, Ordering::Release);
            Ok(())
        }
    }

    #[test]
    fn blocked_camera_io_does_not_block_pub_state_or_heartbeat() {
        let frames = Arc::new(CameraFrames::default());
        let (publisher, _rt, _state, _observers, _ctrl) =
            bring_up_with_camera_and_diag(Some(Arc::clone(&frames)), None);
        assert!(crate::test_util::wait_until(
            Duration::from_secs(2),
            || publisher
                .metrics()
                .heartbeat_published
                .load(Ordering::Relaxed)
                > 0
        ));
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let stopped = Arc::new(AtomicBool::new(false));
        let camera = BlockingCamera {
            entered: Some(entered_tx),
            release: release_rx,
            stopped: Arc::clone(&stopped),
        };
        let io = crate::io_thread::spawn_io_thread_with_cameras(
            crate::io_thread::IoThreadConfig::default(),
            Arc::new(SystemClock),
            Arc::new(AtomicBool::new(false)),
            vec![crate::io_thread::CameraInput {
                adapter: Box::new(camera),
                frames,
            }],
        )
        .unwrap();
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let heartbeat = publisher
            .metrics()
            .heartbeat_published
            .load(Ordering::Relaxed);
        let state = publisher
            .metrics()
            .joint_state_published
            .load(Ordering::Relaxed);
        assert!(crate::test_util::wait_until(
            Duration::from_secs(2),
            || publisher
                .metrics()
                .heartbeat_published
                .load(Ordering::Relaxed)
                > heartbeat
                && publisher
                    .metrics()
                    .joint_state_published
                    .load(Ordering::Relaxed)
                    > state
        ));
        assert_eq!(io.metrics().tick_count.load(Ordering::Relaxed), 0);
        release_tx.send(()).unwrap();
        io.request_stop();
        io.join().unwrap();
        assert!(stopped.load(Ordering::Acquire));
        publisher.request_stop();
        publisher.join().unwrap();
    }

    #[test]
    fn spawn_then_stop_cleanly() {
        let (handle, _rt_pub, _state_pub, _observers, _rt_ctrl) = bring_up();
        thread::sleep(Duration::from_millis(30));
        handle.request_stop();
        handle.join().unwrap();
    }

    /// RT snapshot → `pub_thread` → subscriber 能看到 `JointState` body 的字段。
    #[test]
    fn joint_state_reaches_subscriber() {
        let (handle, mut rt_pub, _state_pub, mut observers, _rt_ctrl) = bring_up();

        // 发一个带 3 关节的 RT snapshot
        let mut jsb = JointStateBuf {
            joint_count: 3,
            stamp_ns: 42_000_000,
            ..JointStateBuf::default()
        };
        jsb.positions[0] = 0.1;
        jsb.positions[1] = 0.2;
        jsb.positions[2] = 0.3;
        // 未标定速度和未测量力矩必须经过快照及真实发布仍为不可用，不能变成0。
        jsb.velocities[1] = f32::NAN;
        jsb.torques[1] = f32::NAN;
        let snap = RtSnapshot {
            state: jsb,
            tick_seq: 99,
            tick_deadline_ns: 42_000_000,
        };
        rt_pub.publish(&snap).unwrap();

        let lease = drain_latest(&mut observers.joint_state, Duration::from_secs(2));
        let view = lease.view().unwrap();
        let fb: FbJointState<'_> = view.body.get();
        assert_eq!(fb.layout_id(), PubThreadConfig::default().joint_layout_id);
        assert_eq!(fb.stamp_ns(), 42_000_000);
        let positions: Vec<f64> = fb.position_rad().unwrap().iter().collect();
        assert_eq!(positions.len(), 3);
        assert!((positions[0] - 0.1).abs() < 1e-3);
        assert!((positions[1] - 0.2).abs() < 1e-3);
        assert!((positions[2] - 0.3).abs() < 1e-3);
        assert!(fb.velocity_rps().unwrap().get(1).is_nan());
        assert!(fb.effort_nm().unwrap().get(1).is_nan());

        handle.request_stop();
        handle.join().unwrap();
    }

    /// 同一权限原子状态变化后，下一帧
    /// `JointState` 的 `s0_safety_state` wire 字段必须同步到对应值。覆盖
    /// 三个有代表性的状态：`StopCommanded`（emergency 后立即态）/
    /// `TorqueOffConfirmed`（confirm 成功）/ `AdapterUncertain`（confirm
    /// timeout）。
    #[test]
    fn safety_state_atomic_propagates_to_wire() {
        use robot_core::executor_ctrl::OutputSafetyState;
        use robot_msg::msg::{JointState as FbJointState, S0SafetyState as WireSafetyState};

        let (handle, mut rt_pub, _state_pub, mut observers, rt_ctrl) = bring_up();

        let cases = [
            (
                OutputSafetyState::StopCommanded,
                WireSafetyState::StopCommanded,
            ),
            (
                OutputSafetyState::TorqueOffConfirmed,
                WireSafetyState::TorqueOffConfirmed,
            ),
            (
                OutputSafetyState::AdapterUncertain,
                WireSafetyState::AdapterUncertain,
            ),
        ];

        for (i, (core_state, wire_expected)) in cases.iter().enumerate() {
            use robot_core::control_authority::{SafetyOperation, SafetyTightening};
            // 此用例只验证同权威到 wire 的映射；不宣称 adapter 或物理断扭验收。
            match core_state {
                OutputSafetyState::StopCommanded => {
                    let _ = rt_ctrl.authority().estop();
                }
                OutputSafetyState::TorqueOffConfirmed => {
                    let observed = rt_ctrl
                        .authority()
                        .begin_safety_observation(SafetyOperation::ConfirmOff);
                    rt_ctrl
                        .authority()
                        .complete_safety_observation(observed)
                        .unwrap();
                }
                OutputSafetyState::AdapterUncertain => {
                    let _ = rt_ctrl
                        .authority()
                        .tighten_safety(SafetyTightening::AdapterUncertain);
                }
                _ => unreachable!("本表只有三个已列映射"),
            }
            // 触发新一帧 publish（snap.tick_seq 唯一让 reader 能区分）。
            #[allow(clippy::cast_possible_truncation)]
            rt_pub
                .publish(&RtSnapshot {
                    state: JointStateBuf {
                        joint_count: 0,
                        stamp_ns: (i as u64 + 1) * 1_000_000,
                        ..JointStateBuf::default()
                    },
                    tick_seq: i as u64 + 1,
                    tick_deadline_ns: 0,
                })
                .unwrap();

            // 轮询直到拿到 stamp_ns 匹配的最新帧 —— 旧帧 stamp_ns 不一致就丢。
            let target_stamp = (i as u64 + 1) * 1_000_000;
            let start = std::time::Instant::now();
            let mut got = false;
            while start.elapsed() < Duration::from_secs(2) {
                if let Ok(Some(lease)) = observers.joint_state.latest()
                    && let Ok(view) = lease.view()
                {
                    let fb: FbJointState<'_> = view.body.get();
                    if fb.stamp_ns() == target_stamp {
                        assert_eq!(
                            fb.s0_safety_state(),
                            *wire_expected,
                            "case {i} core={core_state:?} 应映射到 wire={wire_expected:?}"
                        );
                        got = true;
                        break;
                    }
                }
                thread::sleep(Duration::from_millis(2));
            }
            assert!(got, "case {i} 未观察到 stamp={target_stamp} 帧");
        }

        handle.request_stop();
        handle.join().unwrap();
    }

    /// S0 heartbeat 也按频率发出（`source_id` = 1，seq 单调）。
    #[test]
    fn s0_heartbeat_emitted_with_source_id_1() {
        let (handle, _rt_pub, _state_pub, mut observers, _rt_ctrl) = bring_up();
        let lease = drain_latest(&mut observers.heartbeat, Duration::from_secs(2));
        let view = lease.view().unwrap();
        let fb = view.body.get();
        assert_eq!(fb.source_id(), 1);
        assert!((fb.tick_rate_hz() - 200.0).abs() < 1e-6);

        handle.request_stop();
        handle.join().unwrap();
    }

    /// Metrics 有 `tick_count` 增长 + `joint_state_published` > 0。
    #[test]
    fn metrics_increment_during_run() {
        let (handle, _rt_pub, _state_pub, _observers, _rt_ctrl) = bring_up();
        thread::sleep(Duration::from_millis(60));
        let m = handle.metrics();
        assert!(
            m.tick_count.load(Ordering::Relaxed) > 0,
            "tick_count should have grown"
        );
        assert!(
            m.joint_state_published.load(Ordering::Relaxed) > 0,
            "some JointState should have been published in 60 ms"
        );
        assert!(
            m.heartbeat_published.load(Ordering::Relaxed) > 0,
            "some Heartbeat should have been published in 60 ms"
        );
        handle.request_stop();
        handle.join().unwrap();
    }

    /// `MockClock` 冻结时间：`pub_thread` 应不发任何 `sample`（时间边界永不到期）。
    /// 这验证 `(now - last) >= period` 的边界判断依赖 clock 而非系统时间。
    #[test]
    fn mock_clock_frozen_suppresses_publishes() {
        let topics = fresh_topics();
        let sub_node = NodeBuilder::new().create::<IxService>().unwrap();
        let mut js_sub =
            make_sub::<markers::JointState>(&sub_node, &topics.state, TopicClass::S0State).unwrap();

        let (_rt_pub, rt_rdr) = RtSnapshotPublisher::<RtSnapshot>::new(RtSnapshot::default());
        let (_state_pub, state_rdr) =
            RtSnapshotPublisher::<S0StateSnapshot>::new(S0StateSnapshot::default());

        let mock = MockClock::new(MonoRawNs(0));
        let clock: Arc<dyn RobotClock> = Arc::new(mock);
        let cfg = PubThreadConfig {
            state_hz: 200.0,
            heartbeat_hz: 200.0,
            poll_interval_ns: 1_000_000,
            ..PubThreadConfig::default()
        };

        // 只 open state publisher；heartbeat 走另一份（复用同策略避免 topic 冲突）。
        let state_topic = topics.state.clone();
        let hb_topic = topics.heartbeat.clone();
        let factory = move || {
            let pub_node = NodeBuilder::new().create::<IxService>().unwrap();
            let joint_state =
                make_pub::<markers::JointState>(&pub_node, &state_topic, TopicClass::S0State)
                    .unwrap();
            let heartbeat =
                make_pub::<markers::Heartbeat>(&pub_node, &hb_topic, TopicClass::Heartbeat)
                    .unwrap();
            OwnedIpcPublishers::new(
                IpcPublishers {
                    joint_state,
                    heartbeat,
                },
                pub_node,
            )
        };
        let rt_ctrl = Arc::new(crate::hard_rt_ctrl::HardRtCtrlInner::new(Arc::new(
            robot_core::control_authority::ControlAuthority::new(
                robot_core::control_authority::AuthorityInit::new(
                    false,
                    robot_core::executor_ctrl::OutputSafetyState::TorqueUnknown,
                    robot_core::control_authority::InhibitSet::EMPTY,
                ),
            )
            .unwrap(),
        )));
        let handle = spawn_pub_thread(
            cfg,
            clock,
            factory,
            rt_rdr,
            state_rdr,
            rt_ctrl,
            None,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .unwrap();

        // MockClock 冻结在 0：循环里 now = 0，last_state = 0，`0 - 0 >= period` 永远 false
        // （除非 period == 0；这里 period = 5 ms）。sleep 让 tick 跑几轮。
        thread::sleep(Duration::from_millis(50));

        let m = handle.metrics();
        assert!(m.tick_count.load(Ordering::Relaxed) > 0);
        assert_eq!(m.joint_state_published.load(Ordering::Relaxed), 0);
        // subscriber 看不到 sample
        assert!(matches!(js_sub.latest(), Ok(None)));

        handle.request_stop();
        handle.join().unwrap();

        let _ = MAX_JOINTS; // 保留 import
    }

    /// **A2 freeze hook 单测**：设 `freeze_until_ns` 后，`pub_thread` 的
    /// `tick_count` 在 spin 期间不增长；过期后恢复。`pub_thread` 是 `SoftRT`
    /// publisher 路径，A2-2 control thread 控制路由（worker name → 各自
    /// `freeze_until_ns`）必测之一。
    #[test]
    fn freeze_hook_holds_tick_count() {
        let sys_clock = SystemClock::new();
        let (handle, _rt_pub, _state_pub, _observers, _rt_ctrl) = bring_up();
        let metrics = Arc::clone(handle.metrics());

        // 等 worker 跑起来
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_secs(2)
            && metrics.tick_count.load(Ordering::Relaxed) < 5
        {
            thread::sleep(Duration::from_millis(2));
        }
        let pre_freeze = metrics.tick_count.load(Ordering::Relaxed);
        assert!(pre_freeze >= 5, "warmup 失败 tick={pre_freeze}");

        // 设 freeze 80ms
        let until = sys_clock.now().0 + 80_000_000;
        metrics.freeze_until_ns.store(until, Ordering::Release);

        // freeze 中段
        thread::sleep(Duration::from_millis(40));
        let mid_freeze = metrics.tick_count.load(Ordering::Relaxed);
        let mid_grew = mid_freeze.saturating_sub(pre_freeze);
        assert!(
            mid_grew <= 1,
            "freeze 期间 tick_count 不应继续增长：pre={pre_freeze} mid={mid_freeze} 增 {mid_grew}",
        );

        // freeze 过期后恢复
        thread::sleep(Duration::from_millis(100));
        let post_freeze = metrics.tick_count.load(Ordering::Relaxed);
        assert!(
            post_freeze > mid_freeze + 5,
            "freeze 过期后 tick_count 应当恢复增长：mid={mid_freeze} post={post_freeze}",
        );

        handle.request_stop();
        handle.join().unwrap();
    }
}
