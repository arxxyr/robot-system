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
//! Part 2 没接真 adapter，`JointState.layout_id` 是 `cfg.joint_layout_id` 占位
//! （默认 `0xCAFE_BABE_DEAD_BEEF` 对齐 `hello_pubsub`）。Part 3 接真 adapter 时，
//! `layout_id` 由 adapter 的 `JointLayout` 提供，通过 `cfg` 或额外 reader 传入。

#![forbid(unused_must_use)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use robot_core::adapter::RtSnapshot;
use robot_core::time::RobotClock;
use robot_msg::build_helpers::{build_heartbeat, build_joint_state};
use robot_msg::markers;
use robot_transport::iceoryx2::Iceoryx2Publisher;
use robot_transport::{RtSnapshotReader, traits::Publisher};

use crate::state_machine::S0StateSnapshot;

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
    /// `JointState.layout_id` 占位（Part 2 无真 adapter；Part 3 接真 adapter 时从
    /// `JointLayout` 取）。
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
}

/// 闭包在线程内构造的两个 iceoryx2 publisher。
pub struct IpcPublishers {
    pub joint_state: Iceoryx2Publisher<markers::JointState>,
    pub heartbeat: Iceoryx2Publisher<markers::Heartbeat>,
}

/// `pub_thread` 启动后返回的控制 handle。
pub struct PubThreadHandle {
    join: JoinHandle<()>,
    stop: Arc<AtomicBool>,
    metrics: Arc<PubThreadMetrics>,
}

impl PubThreadHandle {
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
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
/// - `cfg`：发布频率 + `source_id` + `layout_id` 占位。
/// - `clock`：`SystemClock` 生产 / `MockClock` 测试。
/// - `publishers_factory`：`FnOnce` 闭包，在 `pub_thread` 内部 open iceoryx2
///   publisher（对齐 `ipc_thread` 的 subscriber factory；同理 `!Send` 绕过）。
/// - `rt_snap_rdr`：`rt_thread` 发布的 joint state 快照（seqlock 读端）。
/// - `state_rdr`：`ipc_thread` 发布的 state machine 快照（seqlock 读端）。
///
/// # Errors
///
/// `std::io::Error` 当 `thread::Builder::spawn` 失败。
pub fn spawn_pub_thread<F>(
    cfg: PubThreadConfig,
    clock: Arc<dyn RobotClock>,
    publishers_factory: F,
    rt_snap_rdr: RtSnapshotReader<RtSnapshot>,
    state_rdr: RtSnapshotReader<S0StateSnapshot>,
) -> std::io::Result<PubThreadHandle>
where
    F: FnOnce() -> IpcPublishers + Send + 'static,
{
    let stop = Arc::new(AtomicBool::new(false));
    let metrics = Arc::new(PubThreadMetrics::default());

    let join_stop = Arc::clone(&stop);
    let join_metrics = Arc::clone(&metrics);

    // 同步 factory 完成，避免外层测试在 publisher 还没 open 时就 load reader。
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<()>(0);

    let join = thread::Builder::new()
        .name("s0_pub_thread".into())
        .spawn(move || {
            let pubs = publishers_factory();
            let _ = ready_tx.send(());
            drop(ready_tx);
            run_loop(
                &cfg,
                clock.as_ref(),
                pubs,
                &rt_snap_rdr,
                &state_rdr,
                &join_stop,
                &join_metrics,
            );
        })?;

    if ready_rx.recv().is_err() {
        return Err(std::io::Error::other(
            "pub_thread publishers_factory panicked before signalling ready",
        ));
    }

    Ok(PubThreadHandle {
        join,
        stop,
        metrics,
    })
}

// ============================================================================
// 主循环
// ============================================================================

#[allow(clippy::needless_pass_by_value)]
fn run_loop(
    cfg: &PubThreadConfig,
    clock: &dyn RobotClock,
    mut pubs: IpcPublishers,
    rt_snap_rdr: &RtSnapshotReader<RtSnapshot>,
    state_rdr: &RtSnapshotReader<S0StateSnapshot>,
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

    while !stop.load(Ordering::Acquire) {
        let now = clock.now().0;

        // 1) JointState
        if now.saturating_sub(last_state_ns) >= state_period_ns {
            publish_joint_state(
                &mut pubs.joint_state,
                cfg,
                rt_snap_rdr,
                state_rdr,
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

        metrics.tick_count.fetch_add(1, Ordering::Relaxed);
        thread::sleep(Duration::from_nanos(cfg.poll_interval_ns));
    }
}

fn publish_joint_state(
    pub_: &mut Iceoryx2Publisher<markers::JointState>,
    cfg: &PubThreadConfig,
    rt_snap_rdr: &RtSnapshotReader<RtSnapshot>,
    _state_rdr: &RtSnapshotReader<S0StateSnapshot>,
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
    let body = build_joint_state(
        cfg.joint_layout_id,
        snap.state.stamp_ns,
        &positions,
        &velocities,
        &torques,
        "",
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

    struct Topics {
        state: String,
        heartbeat: String,
    }

    fn fresh_topics() -> Topics {
        let id = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let base = format!("/test_pub_{}_{}", std::process::id(), id);
        Topics {
            state: format!("{base}/state"),
            heartbeat: format!("{base}/hb"),
        }
    }

    #[allow(clippy::type_complexity)]
    fn bring_up() -> (
        PubThreadHandle,
        RtSnapshotPublisher<RtSnapshot>,
        RtSnapshotPublisher<S0StateSnapshot>,
        Iceoryx2Subscriber<markers::JointState>,
        Iceoryx2Subscriber<markers::Heartbeat>,
    ) {
        let topics = fresh_topics();
        // subscriber 端 Node 放主线程（测试观察者）。
        let sub_node: &'static _ =
            Box::leak(Box::new(NodeBuilder::new().create::<IxService>().unwrap()));
        let js_sub =
            make_sub::<markers::JointState>(sub_node, &topics.state, TopicClass::S0State).unwrap();
        let hb_sub =
            make_sub::<markers::Heartbeat>(sub_node, &topics.heartbeat, TopicClass::S1CmdVel)
                .unwrap();

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
                make_pub::<markers::Heartbeat>(&pub_node, &hb_topic, TopicClass::S1CmdVel).unwrap();
            Box::leak(Box::new(pub_node));
            IpcPublishers {
                joint_state,
                heartbeat,
            }
        };

        let handle = spawn_pub_thread(cfg, clock, factory, rt_rdr, state_rdr.clone()).unwrap();
        (handle, rt_pub, state_pub, js_sub, hb_sub)
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

    #[test]
    fn spawn_then_stop_cleanly() {
        let (handle, _rt_pub, _state_pub, _js_sub, _hb_sub) = bring_up();
        thread::sleep(Duration::from_millis(30));
        handle.request_stop();
        handle.join().unwrap();
    }

    /// RT snapshot → `pub_thread` → subscriber 能看到 `JointState` body 的字段。
    #[test]
    fn joint_state_reaches_subscriber() {
        let (handle, rt_pub, _state_pub, mut js_sub, _hb_sub) = bring_up();

        // 发一个带 3 关节的 RT snapshot
        let mut jsb = JointStateBuf {
            joint_count: 3,
            stamp_ns: 42_000_000,
            ..JointStateBuf::default()
        };
        jsb.positions[0] = 0.1;
        jsb.positions[1] = 0.2;
        jsb.positions[2] = 0.3;
        let snap = RtSnapshot {
            state: jsb,
            tick_seq: 99,
            tick_deadline_ns: 42_000_000,
        };
        rt_pub.publish(&snap);

        let lease = drain_latest(&mut js_sub, Duration::from_secs(2));
        let view = lease.view().unwrap();
        let fb: FbJointState<'_> = view.body.get();
        assert_eq!(fb.layout_id(), PubThreadConfig::default().joint_layout_id);
        assert_eq!(fb.stamp_ns(), 42_000_000);
        let positions: Vec<f64> = fb.position_rad().unwrap().iter().collect();
        assert_eq!(positions.len(), 3);
        assert!((positions[0] - 0.1).abs() < 1e-3);
        assert!((positions[1] - 0.2).abs() < 1e-3);
        assert!((positions[2] - 0.3).abs() < 1e-3);

        handle.request_stop();
        handle.join().unwrap();
    }

    /// S0 heartbeat 也按频率发出（`source_id` = 1，seq 单调）。
    #[test]
    fn s0_heartbeat_emitted_with_source_id_1() {
        let (handle, _rt_pub, _state_pub, _js_sub, mut hb_sub) = bring_up();
        let lease = drain_latest(&mut hb_sub, Duration::from_secs(2));
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
        let (handle, _rt_pub, _state_pub, _js_sub, _hb_sub) = bring_up();
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
        let sub_node: &'static _ =
            Box::leak(Box::new(NodeBuilder::new().create::<IxService>().unwrap()));
        let mut js_sub =
            make_sub::<markers::JointState>(sub_node, &topics.state, TopicClass::S0State).unwrap();

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
                make_pub::<markers::Heartbeat>(&pub_node, &hb_topic, TopicClass::S1CmdVel).unwrap();
            Box::leak(Box::new(pub_node));
            IpcPublishers {
                joint_state,
                heartbeat,
            }
        };
        let handle = spawn_pub_thread(cfg, clock, factory, rt_rdr, state_rdr).unwrap();

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
}
