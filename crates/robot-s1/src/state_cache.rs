//! `robot-s1::state_cache` —— v0.5 候选 E 段(5)：S1 **常驻** S0 state cache
//! 线程（Spec D3 第 1 条 + P.2.5，R3-B3 契约）。
//!
//! ## 角色
//!
//! 启动锚定（[`crate::coordinator::read_anchor_from_s0_state`]）是"一次性
//! 订阅、读完即弃"（CodeMap round 2 B6）；G1 语义要求 Scripted `start()`
//! 的 segment0 `from` = **提交时当前位**，因此 S1 需要一条**常驻** state
//! subscriber 线程持续更新 latest `JointState`：
//!
//! - 一条 std::thread（`s1_state_cache`），**线程内**创建 iceoryx2 Node +
//!   subscriber（句柄 `!Send`），poll `latest()` → 把最新一帧 **owned body
//!   bytes** 写进 keep-last slot（convert-on-read，沿
//!   `robot-rpc::state_pool` 模板）。
//! - [`StateCacheHandle::latest_fresh`]：校验配置布局、关节数量、有限位置和
//!   `max_stale_ms` 新鲜度，与启动锚定共用 `check_state_frame`。
//! - **失败语义**：无帧 / stale / layout 不匹配 / joint count 错位 / 非有限位置 →
//!   [`StaleOrMissing`] → coordinator 把 Scripted submit 拒为
//!   `ActionStartError::Rejected`（wire `FailedPrecondition`），**绝不**
//!   fallback 到启动 `anchor_bias`（静默跳变危险）。
//!
//! ## 生命周期（AC-E20 线程卫生：stop + join + 正常 Drop，**禁** mem::forget）
//!
//! - [`spawn_state_cache_thread`]：`sync_channel` **同步 ready 握手**——
//!   线程内 node + subscriber 建成后回 `Ok`；建失败回 `Err`；握手超时
//!   （[`READY_TIMEOUT`]）→ [`StateCacheSpawnError::ReadyTimeout`]。**两失败
//!   路径均 stop + join 回滚**后返 `Err`（不留孤儿线程）。**[`READY_TIMEOUT`]
//!   仅是握手超时的检测点，不是 spawn 返回的硬上限**：ready / error 在超时
//!   前到达则立即返回；若先触发超时，则置 stop 并等待 factory / 线程结束后
//!   join——因此超时路径可能超过 5 s，不承诺返回时延硬上限。
//! - [`StateCacheHandle::stop`]（Release flag，幂等）/
//!   [`StateCacheHandle::join`]（一次性，消费 handle）/ `Drop` = stop + join
//!   best-effort。subscriber 与 node 在线程内按 RAII 顺序正常 Drop
//!   （subscriber 先于 node；iceoryx2 0.9.3 上游 #1629 已修 publisher 先退
//!   SIGSEGV，无需 `mem::forget`）。
//! - `robot-s1` 在 cache spawn **成功之后**才打 `rr_server_threads ready`
//!   marker；spawn 失败 = 启动失败 fail-fast，不降级。
//!
//! ## 可测试性
//!
//! [`spawn_state_cache_thread_with`] 是泛型入口：subscriber 由 `factory`
//! 闭包在线程内创建，单元测试用 `robot_transport::mock::MockChannel`
//! 订阅端跑**真线程**（覆盖 ready 握手 / stop / join / Drop / 回滚），
//! 生产入口 [`spawn_state_cache_thread`] 仅是 iceoryx2 factory 的薄封装。

#![allow(clippy::doc_markdown)]

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use core::time::Duration;
use std::sync::mpsc::{RecvTimeoutError, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};

use iceoryx2::node::Node as IxNode;
use iceoryx2::prelude::NodeBuilder;
use iceoryx2::service::ipc::Service as IxService;
use robot_core::time::{DurationNs, MonoRawNs, RobotClock};
use robot_msg::markers;
use robot_transport::error::TransportError;
use robot_transport::iceoryx2::{Iceoryx2Subscriber, TopicClass, subscriber as open_sub};
use robot_transport::sample::SampleLease;
use robot_transport::traits::LatestSubscriber;
use thiserror::Error;

use crate::coordinator::{FrameCheckError, check_state_frame};

// ============================================================================
// 常量
// ============================================================================

/// ready 握手超时**检测点**（[`spawn_state_cache_thread`]）——**不是** spawn
/// 返回的硬上限。iceoryx2 node create + subscriber open 在 anchor 成功后是
/// 毫秒级：ready / error 在超时前到达则立即返回；若先触发超时，则返
/// [`StateCacheSpawnError::ReadyTimeout`] 之前置 stop 并等待 factory / 线程
/// 结束后 `join`（保证无孤儿线程）——因此超时路径可能超过 5 s，不承诺返回
/// 时延硬上限。
pub const READY_TIMEOUT: Duration = Duration::from_secs(5);

/// 线程内 transport / view 错误的限频日志：首条必报 + 每 256 条一报
/// （沿 S0 command gate `unknown_layout_count` 限频先例，避免刷屏）。
const ERROR_LOG_EVERY: u64 = 256;

// ============================================================================
// StateCacheConfig
// ============================================================================

/// [`spawn_state_cache_thread`] 参数（R3-B3 契约字段集）。
///
/// - `topic`：`/<robot_id>/s0/<hardware_id>/state`（与启动锚定同 topic）。
/// - `expected_joints`：`hardware.joints.len()`。
/// - `expected_layout`：S1 config 派生 `layout_id`（段(4) 唯一权威）。
/// - `max_stale_ms`：沿既有 `[s1].max_stale_ms`（D3 子拍板：不新增配置面）。
/// - `poll_interval`：subscriber 线程**标称** polling cadence（robot-s1 用
///   2 ms，沿 `robot-rpc::state_pool` 口径）；普通线程调度无硬上界，**不**
///   构成新鲜度保证——数据安全由帧 `stamp_ns` + `max_stale_ms` 在读路径
///   （[`StateCacheHandle::latest_fresh`]）守门。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateCacheConfig {
    pub topic: String,
    pub expected_joints: usize,
    pub expected_layout: u64,
    pub max_stale_ms: u64,
    pub poll_interval: Duration,
}

impl StateCacheConfig {
    /// `max_stale_ms` → ns（饱和乘，绝不 wrap）。
    #[inline]
    #[must_use]
    pub fn max_stale_ns(&self) -> u64 {
        self.max_stale_ms.saturating_mul(1_000_000)
    }
}

// ============================================================================
// 错误类型
// ============================================================================

/// [`spawn_state_cache_thread`] 失败原因（四路，R3-B3 契约）。任一路径返回
/// 时线程均已 stop + join（无孤儿线程）。
#[derive(Debug, Error)]
pub enum StateCacheSpawnError {
    /// 线程内 iceoryx2 `NodeBuilder::create` 失败。
    #[error("state cache: iceoryx2 node create failed: {0}")]
    NodeCreate(TransportError),
    /// 线程内 subscriber open 失败（service create / 配额等）。
    #[error("state cache: subscriber open failed: {0}")]
    SubscriberOpen(TransportError),
    /// `std::thread::Builder::spawn` 失败（罕见；OS 线程资源）。
    #[error("state cache: thread spawn failed: {0}")]
    ThreadSpawn(std::io::Error),
    /// ready 握手在 [`READY_TIMEOUT`]（或测试入口显式传入的超时）内未收到
    /// 线程回执。返回前已 join 线程（等 factory 返回），故 spawn 实际耗时
    /// 可能远超该超时（检测点，非硬上限）。
    #[error(
        "state cache: ready handshake timed out (thread did not report subscriber open in time)"
    )]
    ReadyTimeout,
}

/// [`StateCacheHandle::latest_fresh`] 失败原因（"stale or missing"）——
/// coordinator 拒 Scripted submit 的依据。每 variant 单因；`Display`
/// 文案直接进 `ActionStartError::Rejected` 消息供 wire 透传。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum StaleOrMissing {
    /// subscriber 已 open 但尚未收到任何帧。
    #[error("state cache has no frame yet (no S0 state received since subscriber open)")]
    NoFrame,
    /// 最新帧 `stamp_ns == 0` 或比 `now` 旧超过 `max_stale_ms`。
    #[error(
        "S0 state sample is stale: stamp_ns={stamp_ns}, now_ns={now_ns}, max_stale_ns={max_stale_ns}"
    )]
    Stale {
        stamp_ns: u64,
        now_ns: u64,
        max_stale_ns: u64,
    },
    /// `JointState.layout_id` 与 S1 config 派生值不等（D1 联动）。
    #[error("layout_id mismatch: S1 config derives {expected:#018x}, S0 state carries {got:#018x}")]
    LayoutMismatch { expected: u64, got: u64 },
    /// `position_rad.len()` 与 `hardware.joints.len()` 不等。
    #[error("joint count mismatch: hardware section expects {expected}, S0 state has {got}")]
    JointCountMismatch { expected: usize, got: usize },
    /// `position_rad` 首个非有限数值的下标。
    #[error("S0 position_rad[{index}] is not finite")]
    NonFinitePosition { index: usize },
    /// body 无 `position_rad` 字段（publisher bug）。
    #[error("S0 state sample has no position_rad field")]
    MissingPositionField,
    /// fbs verify / root 解码失败。
    #[error("S0 state body decode failed: {0}")]
    Decode(String),
}

impl From<FrameCheckError> for StaleOrMissing {
    fn from(e: FrameCheckError) -> Self {
        match e {
            FrameCheckError::Decode(s) => Self::Decode(s),
            FrameCheckError::LayoutMismatch { expected, got } => {
                Self::LayoutMismatch { expected, got }
            }
            FrameCheckError::Stale {
                stamp_ns,
                now_ns,
                max_stale_ns,
            } => Self::Stale {
                stamp_ns,
                now_ns,
                max_stale_ns,
            },
            FrameCheckError::MissingPositionField => Self::MissingPositionField,
            FrameCheckError::JointCountMismatch { expected, got } => {
                Self::JointCountMismatch { expected, got }
            }
            FrameCheckError::NonFinitePosition { index } => Self::NonFinitePosition { index },
        }
    }
}

// ============================================================================
// CacheSlot（线程 ↔ handle 共享；keep-last owned body bytes）
// ============================================================================

/// 最新一帧（owned body bytes + 收到时刻）。
#[derive(Debug)]
struct CachedFrame {
    body: Vec<u8>,
    received_at: MonoRawNs,
}

/// keep-last slot + 计数器。`body` `Vec` 跨帧复用容量（50 Hz 下不重分配）。
#[derive(Debug, Default)]
struct CacheSlot {
    latest: Mutex<Option<CachedFrame>>,
    frames_received: AtomicU64,
    transport_errors: AtomicU64,
}

/// state cache 线程 liveness 面（v0.5 候选 E 段(8)，Spec D12）：`Arc` 由
/// handle 与线程共享，watchdog observer / test-only freeze control 经
/// [`StateCacheHandle::liveness`] 取得。
#[derive(Debug, Default)]
pub struct StateCacheLiveness {
    /// 每次 poll 迭代 +1（空闲亦推进；liveness 源）。
    pub loop_tick_count: AtomicU64,
    /// test-only：loop 顶端 freeze 截止（absolute ns，0 = 未冻结）。
    #[cfg(any(test, feature = "test-freeze-hook"))]
    pub freeze_until_ns: AtomicU64,
}

impl CacheSlot {
    fn lock_latest(&self) -> MutexGuard<'_, Option<CachedFrame>> {
        self.latest.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// subscriber 线程调：keep-last 覆盖（复用 `Vec` 容量）。
    fn store(&self, raw: &[u8], received_at: MonoRawNs) {
        {
            let mut guard = self.lock_latest();
            match guard.as_mut() {
                Some(frame) => {
                    frame.body.clear();
                    frame.body.extend_from_slice(raw);
                    frame.received_at = received_at;
                }
                None => {
                    *guard = Some(CachedFrame {
                        body: raw.to_vec(),
                        received_at,
                    });
                }
            }
        }
        self.frames_received.fetch_add(1, Ordering::Release);
    }
}

/// [`StateCacheHandle::stats`] 快照（诊断 / 测试）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StateCacheStats {
    /// 线程写入 slot 的帧数（keep-last 覆盖计数，不是 distinct 帧数）。
    pub frames_received: u64,
    /// 最新一帧写入 slot 的时刻（线程 `clock.now()`）；无帧 = `None`。
    pub last_received_at: Option<MonoRawNs>,
    /// `latest()` / `view()` 错误累计（限频 warn，保留旧帧）。
    pub transport_errors: u64,
}

// ============================================================================
// StateCacheHandle
// ============================================================================

/// 常驻 state cache 线程 handle（owner = [`crate::coordinator::Coordinator`]
/// 经 `with_state_cache` 持有；robot-s1 shutdown 时 `take_state_cache` →
/// `stop` → `join`）。
///
/// **Drop** = `stop` + best-effort `join`（AC-E20：任何路径都不留孤儿线程、
/// 不 `mem::forget`）。
pub struct StateCacheHandle {
    cfg: StateCacheConfig,
    slot: Arc<CacheSlot>,
    liveness: Arc<StateCacheLiveness>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl core::fmt::Debug for StateCacheHandle {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // `slot` 内容以 `stats()` 快照呈现（不 dump body bytes）；
        // `thread` 以 joined 布尔呈现 → non_exhaustive。
        f.debug_struct("StateCacheHandle")
            .field("cfg", &self.cfg)
            .field("stopped", &self.stop.load(Ordering::Acquire))
            .field("joined", &self.thread.is_none())
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

/// 同一帧中的位置、源时间及运动准入状态；不是控制命令或扭矩操作。
#[derive(Debug, Clone, PartialEq)]
pub struct JointFeedbackSnapshot {
    pub stamp_ns: u64,
    pub positions: Vec<f64>,
    pub motion_ready: bool,
}

impl StateCacheHandle {
    /// 借用 spawn 时的配置（topic / expected_* / max_stale_ms / poll）。
    #[must_use]
    pub fn config(&self) -> &StateCacheConfig {
        &self.cfg
    }

    /// 读路径：以 `now`（调用方时钟，与 S0 同源 `CLOCK_MONOTONIC_RAW`）
    /// 评估最新帧 → 全部校验通过后返回有限关节位置。
    ///
    /// 校验 = crate 私有 `check_state_frame`（与启动锚定共享）+
    /// `expected_layout = cfg.expected_layout`：
    /// 无帧 → [`StaleOrMissing::NoFrame`]；解码 / layout / stale /
    /// 缺字段 / joint count / 非有限位置依次短路（详 `check_state_frame` doc）。
    ///
    /// # Errors
    ///
    /// 任一校验失败返 [`StaleOrMissing`]；调用方（coordinator）**不得**
    /// fallback 到 `anchor_bias`（Spec D3 第 1 条）。
    pub fn latest_fresh(&self, now: MonoRawNs) -> Result<Vec<f64>, StaleOrMissing> {
        self.read_fresh_feedback(|| now)
            .map(|(snapshot, _)| snapshot.positions)
    }

    /// 返回经过同一布局、新鲜度及有限值检查的完整反馈快照。
    pub fn latest_fresh_snapshot(
        &self,
        clock: &dyn RobotClock,
    ) -> Result<JointFeedbackSnapshot, StaleOrMissing> {
        self.read_fresh_feedback(|| clock.now())
            .map(|(snapshot, _)| snapshot)
    }

    /// 仅同帧确认 Safe、断扭及无故障时提供使能前的位置对齐快照。
    pub(crate) fn latest_torque_off_positions(
        &self,
        clock: &dyn RobotClock,
    ) -> Result<Option<Vec<f64>>, StaleOrMissing> {
        self.read_fresh_feedback(|| clock.now())
            .map(|(snapshot, torque_off)| torque_off.then_some(snapshot.positions))
    }

    fn read_fresh_feedback(
        &self,
        read_time: impl FnOnce() -> MonoRawNs,
    ) -> Result<(JointFeedbackSnapshot, bool), StaleOrMissing> {
        use robot_msg::msg::{
            JointState, S0ControlLink, S0FaultCode, S0Mode, S0OutputMode, S0SafetyState,
        };

        let guard = self.slot.lock_latest();
        let Some(frame) = guard.as_ref() else {
            return Err(StaleOrMissing::NoFrame);
        };
        // 先固定帧，再读当前时间，避免并发新帧被旧的调用时刻误判为未来帧。
        let now = read_time();
        let positions = check_state_frame(
            &frame.body,
            self.cfg.expected_joints,
            self.cfg.expected_layout,
            now.0,
            self.cfg.max_stale_ns(),
        )
        .map_err(StaleOrMissing::from)?;
        let state = flatbuffers::root::<JointState<'_>>(&frame.body)
            .map_err(|error| StaleOrMissing::Decode(error.to_string()))?;
        let snapshot = JointFeedbackSnapshot {
            stamp_ns: state.stamp_ns(),
            positions,
            motion_ready: state.s0_mode() == S0Mode::Enabled
                && state.s0_control_link() == S0ControlLink::Ok
                && state.s0_output_mode() == S0OutputMode::Active
                && state.s0_safety_state() == S0SafetyState::TorqueOn
                && state.s0_fault_code() == S0FaultCode::Unknown,
        };
        let torque_off = state.s0_mode() == S0Mode::Safe
            && state.s0_safety_state() == S0SafetyState::TorqueOffConfirmed
            && state.s0_fault_code() == S0FaultCode::Unknown;
        Ok((snapshot, torque_off))
    }

    /// 线程 liveness 面（watchdog observer 读 `loop_tick_count`；test-only
    /// freeze control 写 `freeze_until_ns`）。
    #[must_use]
    pub fn liveness(&self) -> Arc<StateCacheLiveness> {
        Arc::clone(&self.liveness)
    }

    /// 计数快照（诊断 / 测试等待首帧）。
    #[must_use]
    pub fn stats(&self) -> StateCacheStats {
        let last_received_at = self.slot.lock_latest().as_ref().map(|f| f.received_at);
        StateCacheStats {
            frames_received: self.slot.frames_received.load(Ordering::Acquire),
            last_received_at,
            transport_errors: self.slot.transport_errors.load(Ordering::Acquire),
        }
    }

    /// 信号线程退出（Release flag；幂等、不阻塞）。线程在下一次 poll 检查
    /// flag 时退出（标称 ≤ `cfg.poll_interval` + 一次 `latest()`；普通线程
    /// 调度无硬上界）；需要确定性等待用 [`Self::join`]。
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    /// `stop` + join（一次性，消费 handle）。返线程 join 结果（`Err` =
    /// 线程 panic payload；线程体本身无 panic 路径，保留给调用方 log）。
    ///
    /// # Errors
    ///
    /// 线程 panic 时返 `Err(payload)`（沿 `std::thread::JoinHandle::join`）。
    pub fn join(mut self) -> thread::Result<()> {
        self.stop();
        match self.thread.take() {
            Some(handle) => handle.join(),
            None => Ok(()),
        }
    }
}

impl Drop for StateCacheHandle {
    fn drop(&mut self) {
        // best-effort：stop 幂等；已 join（thread == None）则无事可做。
        self.stop();
        if let Some(handle) = self.thread.take()
            && let Err(payload) = handle.join()
        {
            tracing::error!(
                target: "robot_s1::state_cache",
                ?payload,
                "state cache thread join returned panic during Drop"
            );
        }
    }
}

// ============================================================================
// spawn（泛型入口 + iceoryx2 生产入口）
// ============================================================================

/// 泛型 spawn：subscriber 由 `factory` **在线程内**创建（iceoryx2 句柄
/// `!Send`；mock 亦走同路径保证单测覆盖真线程生命周期）。
///
/// 流程：spawn 线程 → 线程内 `factory()` → `sync_channel(1)` 回 ready
/// 结果 → 主线程 `recv_timeout(ready_timeout)`：
///
/// - `Ok(Ok(()))` → 返 handle（线程进入 poll loop）。
/// - `Ok(Err(e))` → 线程已自行退出；join 后返 `Err(e)`。
/// - `Err(Timeout)` → `stop` → drop 接收端（线程回执 `send` 立即失败不阻塞）
///   → `join` → [`StateCacheSpawnError::ReadyTimeout`]。**`join` 等 factory
///   返回**：本函数总耗时不受 `ready_timeout` 硬约束（无孤儿线程优先）。
/// - `Err(Disconnected)`（线程在回执前 panic）→ join 拿 panic 后按
///   `ReadyTimeout` 同路径回滚（线程已死，无需 stop）。
///
/// # Errors
///
/// 详 [`StateCacheSpawnError`]；任一 `Err` 返回时线程均已 join。
pub fn spawn_state_cache_thread_with<S, F, C>(
    cfg: StateCacheConfig,
    clock: Arc<C>,
    ready_timeout: Duration,
    factory: F,
) -> Result<StateCacheHandle, StateCacheSpawnError>
where
    S: LatestSubscriber<markers::JointState> + 'static,
    F: FnOnce() -> Result<S, StateCacheSpawnError> + Send + 'static,
    C: RobotClock + 'static,
{
    let slot = Arc::new(CacheSlot::default());
    let liveness = Arc::new(StateCacheLiveness::default());
    let stop = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = sync_channel::<Result<(), StateCacheSpawnError>>(1);

    let slot_for_thread = Arc::clone(&slot);
    let liveness_for_thread = Arc::clone(&liveness);
    let stop_for_thread = Arc::clone(&stop);
    let poll_ns = u64::try_from(cfg.poll_interval.as_nanos()).unwrap_or(u64::MAX);
    let topic_for_thread = cfg.topic.clone();

    let handle = thread::Builder::new()
        .name("s1_state_cache".into())
        .spawn(move || {
            let mut sub = match factory() {
                Ok(s) => s,
                Err(e) => {
                    // 建失败：回执后直接退出（主线程 join 回滚）。
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            // 回执 Ok；接收端已超时丢弃时 send 返 Err（buffer=1 不阻塞）→
            // 紧接着的 stop 检查让线程退出。
            let _ = ready_tx.send(Ok(()));
            drop(ready_tx);

            run_cache_loop(
                &mut sub,
                &slot_for_thread,
                &liveness_for_thread,
                &stop_for_thread,
                clock.as_ref(),
                poll_ns,
                &topic_for_thread,
            );
            // `sub`（含 iceoryx2 node）在此按 RAII 正常 Drop（AC-E20）。
        })
        .map_err(StateCacheSpawnError::ThreadSpawn)?;

    match ready_rx.recv_timeout(ready_timeout) {
        Ok(Ok(())) => Ok(StateCacheHandle {
            cfg,
            slot,
            liveness,
            stop,
            thread: Some(handle),
        }),
        Ok(Err(e)) => {
            // 线程已自行 return；join 收尸后返 Err（不留孤儿）。
            stop.store(true, Ordering::Release);
            let _ = handle.join();
            Err(e)
        }
        Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
            // 超时 / 线程回执前死亡：stop → 先 drop 接收端（线程若随后
            // 建成，其 send 立即失败而非阻塞）→ join 回滚。
            stop.store(true, Ordering::Release);
            drop(ready_rx);
            let _ = handle.join();
            Err(StateCacheSpawnError::ReadyTimeout)
        }
    }
}

/// 线程 poll loop：`latest()` 有帧 → `view()` → 写 slot；无帧 / 错误 →
/// 保留旧帧（keep-last 持久）；错误限频 warn。`stop` 置位后于下一周期退出。
fn run_cache_loop<S>(
    sub: &mut S,
    slot: &CacheSlot,
    liveness: &StateCacheLiveness,
    stop: &AtomicBool,
    clock: &dyn RobotClock,
    poll_ns: u64,
    topic: &str,
) where
    S: LatestSubscriber<markers::JointState>,
{
    while !stop.load(Ordering::Acquire) {
        // v0.5 候选 E 段(8)：liveness 计数（空闲亦推进）+ test-only freeze hook。
        liveness.loop_tick_count.fetch_add(1, Ordering::Relaxed);
        #[cfg(any(test, feature = "test-freeze-hook"))]
        crate::liveness::freeze_check(&liveness.freeze_until_ns, clock, stop);
        match sub.latest() {
            Ok(Some(lease)) => store_lease(&lease, slot, clock, topic),
            Ok(None) => {}
            Err(e) => note_transport_error(slot, topic, &e),
        }
        clock.sleep_for(DurationNs::from_nanos(poll_ns));
    }
}

/// 一帧 lease → `view()` → 写 slot（body raw bytes；convert-on-read）。
fn store_lease(
    lease: &SampleLease<markers::JointState>,
    slot: &CacheSlot,
    clock: &dyn RobotClock,
    topic: &str,
) {
    match lease.view() {
        Ok(view) => slot.store(view.body.raw(), clock.now()),
        Err(e) => note_transport_error(slot, topic, &e),
    }
}

/// 计数 + 限频 warn（首条必报 + 每 [`ERROR_LOG_EVERY`] 条一报）。
fn note_transport_error(slot: &CacheSlot, topic: &str, e: &TransportError) {
    let n = slot.transport_errors.fetch_add(1, Ordering::AcqRel) + 1;
    if n == 1 || n.is_multiple_of(ERROR_LOG_EVERY) {
        tracing::warn!(
            target: "robot_s1::state_cache",
            topic,
            transport_errors = n,
            error = %e,
            "state cache subscriber error (keeping last frame)"
        );
    }
}

/// iceoryx2 subscriber + 其 node 的 RAII 包：字段声明顺序 = drop 顺序
/// （`sub` 先于 `node`），线程退出时两者正常析构，**不** `Box::leak` /
/// `mem::forget`。
struct Iceoryx2StateSubscriber {
    sub: Iceoryx2Subscriber<markers::JointState>,
    _node: IxNode<IxService>,
}

impl LatestSubscriber<markers::JointState> for Iceoryx2StateSubscriber {
    fn latest(&mut self) -> Result<Option<SampleLease<markers::JointState>>, TransportError> {
        self.sub.latest()
    }
}

/// 生产入口：线程内创建 iceoryx2 Node + `TopicClass::S0State` subscriber，
/// ready 握手超时检测点 = [`READY_TIMEOUT`]（回滚 join 等 factory 返回，
/// 总耗时非 5 s 硬上限）。
///
/// # Errors
///
/// 详 [`StateCacheSpawnError`]（`NodeCreate` / `SubscriberOpen` /
/// `ThreadSpawn` / `ReadyTimeout`）；任一路径线程已 join。
pub fn spawn_state_cache_thread<C: RobotClock + 'static>(
    cfg: StateCacheConfig,
    clock: Arc<C>,
) -> Result<StateCacheHandle, StateCacheSpawnError> {
    let topic = cfg.topic.clone();
    spawn_state_cache_thread_with(cfg, clock, READY_TIMEOUT, move || {
        let node = NodeBuilder::new().create::<IxService>().map_err(|e| {
            StateCacheSpawnError::NodeCreate(TransportError::Backend(format!(
                "iceoryx2 node create: {e:?}"
            )))
        })?;
        let sub = open_sub::<markers::JointState>(&node, &topic, TopicClass::S0State)
            .map_err(StateCacheSpawnError::SubscriberOpen)?;
        Ok(Iceoryx2StateSubscriber { sub, _node: node })
    })
}

// ============================================================================
// Tests（mock transport 跑真线程；iceoryx2 路径由 robot-s1 + smoke 覆盖）
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    use robot_core::time::SystemClock;
    use robot_msg::build_helpers::build_joint_state;
    use robot_transport::mock::{MockChannel, MockSubscriber};
    use robot_transport::traits::Publisher;

    const LAYOUT: u64 = 0x1122_3344_5566_7788;
    const JOINTS: usize = 3;
    const POLL: Duration = Duration::from_millis(1);

    fn cfg() -> StateCacheConfig {
        StateCacheConfig {
            topic: "/t/s0/arm/state".into(),
            expected_joints: JOINTS,
            expected_layout: LAYOUT,
            max_stale_ms: 200,
            poll_interval: POLL,
        }
    }

    fn publish_state(
        ch: &MockChannel<markers::JointState>,
        layout: u64,
        positions: &[f64],
        stamp_ns: u64,
    ) {
        let body = build_joint_state(layout, stamp_ns, positions, &[], &[], "");
        let mut pub_ = ch.publisher();
        let mut w = pub_.loan(body.len() + 1024).expect("loan");
        w.set_body_bytes(&body);
        w.set_stamp_ns(stamp_ns);
        w.set_seq(0);
        w.set_source_id(1);
        w.publish().expect("publish");
    }

    /// 起真线程 + mock subscriber。
    fn spawn_mock(
        ch: &MockChannel<markers::JointState>,
        cfg: StateCacheConfig,
    ) -> StateCacheHandle {
        let ch = ch.clone();
        spawn_state_cache_thread_with(
            cfg,
            Arc::new(SystemClock::new()),
            READY_TIMEOUT,
            move || Ok(ch.subscriber()),
        )
        .expect("spawn ok")
    }

    /// 等线程把 ≥ `min_frames` 帧写进 slot（真线程 1 ms poll；上限 2 s）。
    fn wait_frames(handle: &StateCacheHandle, min_frames: u64) {
        for _ in 0..2000 {
            if handle.stats().frames_received >= min_frames {
                return;
            }
            thread::sleep(Duration::from_millis(1));
        }
        panic!(
            "state cache thread did not reach {min_frames} frames: {:?}",
            handle.stats()
        );
    }

    // ----- ready 握手 + 读路径 -----

    #[test]
    fn torque_off_positions_require_same_frame_safe_confirmation_and_no_fault() {
        use robot_core::time::MockClock;
        use robot_msg::build_helpers::build_joint_state_with_s0;
        use robot_msg::msg::{S0ControlLink, S0FaultCode, S0Mode, S0OutputMode, S0SafetyState};

        let handle = StateCacheHandle {
            cfg: cfg(),
            slot: Arc::new(CacheSlot::default()),
            liveness: Arc::new(StateCacheLiveness::default()),
            stop: Arc::new(AtomicBool::new(false)),
            thread: None,
        };
        let now = MonoRawNs(5_000_000_000);
        let clock = MockClock::new(now);
        let states = [
            (
                S0Mode::Safe,
                S0SafetyState::TorqueOffConfirmed,
                S0FaultCode::Unknown,
                true,
            ),
            (
                S0Mode::Enabled,
                S0SafetyState::TorqueOffConfirmed,
                S0FaultCode::Unknown,
                false,
            ),
            (
                S0Mode::Unknown,
                S0SafetyState::TorqueOffConfirmed,
                S0FaultCode::Unknown,
                false,
            ),
            (
                S0Mode::Safe,
                S0SafetyState::StopCommanded,
                S0FaultCode::Unknown,
                false,
            ),
            (
                S0Mode::Safe,
                S0SafetyState::TorqueOn,
                S0FaultCode::Unknown,
                false,
            ),
            (
                S0Mode::Safe,
                S0SafetyState::TorqueUnknown,
                S0FaultCode::Unknown,
                false,
            ),
            (
                S0Mode::Safe,
                S0SafetyState::TorqueOffConfirmed,
                S0FaultCode(1),
                false,
            ),
        ];
        for (mode, safety, fault, accepted) in states {
            let body = build_joint_state_with_s0(
                LAYOUT,
                now.0,
                &[0.25; JOINTS],
                &[],
                &[],
                "",
                mode,
                S0ControlLink::Unknown,
                S0OutputMode::Unknown,
                safety,
                fault,
            );
            handle.slot.store(&body, now);
            assert_eq!(
                handle.latest_torque_off_positions(&clock).unwrap(),
                accepted.then_some(vec![0.25; JOINTS]),
                "mode={mode:?}, safety={safety:?}, fault={fault:?}",
            );
            assert!(!handle.latest_fresh_snapshot(&clock).unwrap().motion_ready);
        }
    }

    #[test]
    fn spawn_ready_then_latest_fresh_returns_positions() {
        let ch = MockChannel::<markers::JointState>::new();
        let handle = spawn_mock(&ch, cfg());
        assert_eq!(handle.stats().frames_received, 0);

        let now: u64 = 5_000_000_000;
        publish_state(&ch, LAYOUT, &[0.1, -0.2, 0.3], now);
        wait_frames(&handle, 1);

        let got = handle
            .latest_fresh(MonoRawNs(now + 10_000_000))
            .expect("fresh");
        assert_eq!(got, vec![0.1, -0.2, 0.3]);
        assert!(handle.stats().last_received_at.is_some());

        handle.join().expect("join ok");
    }

    #[test]
    fn latest_fresh_no_frame_before_first_publish() {
        let ch = MockChannel::<markers::JointState>::new();
        let handle = spawn_mock(&ch, cfg());
        let err = handle.latest_fresh(MonoRawNs(1)).unwrap_err();
        assert_eq!(err, StaleOrMissing::NoFrame);
        handle.join().expect("join ok");
    }

    #[test]
    fn latest_fresh_rejects_layout_mismatch() {
        let ch = MockChannel::<markers::JointState>::new();
        let handle = spawn_mock(&ch, cfg());
        let now: u64 = 5_000_000_000;
        publish_state(&ch, LAYOUT ^ 0xFF, &[0.0; JOINTS], now);
        wait_frames(&handle, 1);
        let err = handle.latest_fresh(MonoRawNs(now)).unwrap_err();
        assert!(
            matches!(err, StaleOrMissing::LayoutMismatch { expected, got }
                if expected == LAYOUT && got == LAYOUT ^ 0xFF),
            "got {err:?}"
        );
        handle.join().expect("join ok");
    }

    #[test]
    fn latest_fresh_rejects_nonfinite_latest_instead_of_reusing_valid_frame() {
        let handle = StateCacheHandle {
            cfg: cfg(),
            slot: Arc::new(CacheSlot::default()),
            liveness: Arc::new(StateCacheLiveness::default()),
            stop: Arc::new(AtomicBool::new(false)),
            thread: None,
        };
        let now = MonoRawNs(5_000_000_000);
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let valid = build_joint_state(LAYOUT, now.0, &[0.1; JOINTS], &[], &[], "");
            handle.slot.store(&valid, now);
            assert_eq!(handle.latest_fresh(now).unwrap(), vec![0.1; JOINTS]);
            let invalid = build_joint_state(LAYOUT, now.0, &[0.1, value, 0.1], &[], &[], "");
            handle.slot.store(&invalid, now);
            assert_eq!(
                handle.latest_fresh(now),
                Err(StaleOrMissing::NonFinitePosition { index: 1 })
            );
        }
    }

    #[test]
    fn latest_fresh_rejects_stale_frame() {
        let ch = MockChannel::<markers::JointState>::new();
        let handle = spawn_mock(&ch, cfg());
        let stamp: u64 = 5_000_000_000;
        publish_state(&ch, LAYOUT, &[0.0; JOINTS], stamp);
        wait_frames(&handle, 1);
        // now = stamp + 201 ms > max_stale 200 ms → Stale。
        let now = stamp + 201_000_000;
        let err = handle.latest_fresh(MonoRawNs(now)).unwrap_err();
        assert!(
            matches!(err, StaleOrMissing::Stale { stamp_ns, now_ns, max_stale_ns }
                if stamp_ns == stamp && now_ns == now && max_stale_ns == 200_000_000),
            "got {err:?}"
        );
        // 边界：恰好 200 ms 不 stale。
        handle
            .latest_fresh(MonoRawNs(stamp + 200_000_000))
            .expect("boundary fresh");
        handle.join().expect("join ok");
    }

    #[test]
    fn latest_fresh_rejects_joint_count_mismatch() {
        let ch = MockChannel::<markers::JointState>::new();
        let handle = spawn_mock(&ch, cfg());
        let now: u64 = 5_000_000_000;
        publish_state(&ch, LAYOUT, &[0.0, 0.0], now);
        wait_frames(&handle, 1);
        let err = handle.latest_fresh(MonoRawNs(now)).unwrap_err();
        assert!(
            matches!(
                err,
                StaleOrMissing::JointCountMismatch {
                    expected: JOINTS,
                    got: 2
                }
            ),
            "got {err:?}"
        );
        handle.join().expect("join ok");
    }

    #[test]
    fn latest_fresh_layout_check_precedes_stale_check() {
        // 配置级错误优先于时序级错误：layout 错 + stale 同时成立 → LayoutMismatch。
        let ch = MockChannel::<markers::JointState>::new();
        let handle = spawn_mock(&ch, cfg());
        let stamp: u64 = 1_000_000_000;
        publish_state(&ch, LAYOUT ^ 1, &[0.0; JOINTS], stamp);
        wait_frames(&handle, 1);
        let err = handle
            .latest_fresh(MonoRawNs(stamp + 10_000_000_000))
            .unwrap_err();
        assert!(
            matches!(err, StaleOrMissing::LayoutMismatch { .. }),
            "got {err:?}"
        );
        handle.join().expect("join ok");
    }

    #[test]
    fn keep_last_frame_wins() {
        let ch = MockChannel::<markers::JointState>::new();
        let handle = spawn_mock(&ch, cfg());
        let now: u64 = 5_000_000_000;
        publish_state(&ch, LAYOUT, &[1.0; JOINTS], now);
        wait_frames(&handle, 1);
        publish_state(&ch, LAYOUT, &[2.0; JOINTS], now + 1);
        // 第二帧被 poll 到后 latest_fresh 必返 [2.0; 3]（keep-last）。
        let mut got = Vec::new();
        for _ in 0..2000 {
            got = handle.latest_fresh(MonoRawNs(now + 2)).expect("fresh");
            if got == vec![2.0; JOINTS] {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(got, vec![2.0; JOINTS]);
        handle.join().expect("join ok");
    }

    // ----- AC-E20：stop + join + Drop 线程卫生 -----

    /// Drop 探针：包一层 mock subscriber，析构时置 flag —— 证明线程退出
    /// 时 subscriber 真正 Drop（非 mem::forget）。
    struct DropProbeSub {
        inner: MockSubscriber<markers::JointState>,
        dropped: Arc<AtomicBool>,
    }

    impl LatestSubscriber<markers::JointState> for DropProbeSub {
        fn latest(&mut self) -> Result<Option<SampleLease<markers::JointState>>, TransportError> {
            self.inner.latest()
        }
    }

    impl Drop for DropProbeSub {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Release);
        }
    }

    fn spawn_probe(ch: &MockChannel<markers::JointState>) -> (StateCacheHandle, Arc<AtomicBool>) {
        let dropped = Arc::new(AtomicBool::new(false));
        let ch = ch.clone();
        let flag = Arc::clone(&dropped);
        let handle = spawn_state_cache_thread_with(
            cfg(),
            Arc::new(SystemClock::new()),
            READY_TIMEOUT,
            move || {
                Ok(DropProbeSub {
                    inner: ch.subscriber(),
                    dropped: flag,
                })
            },
        )
        .expect("spawn ok");
        (handle, dropped)
    }

    #[test]
    fn explicit_stop_then_join_drops_subscriber() {
        let ch = MockChannel::<markers::JointState>::new();
        let (handle, dropped) = spawn_probe(&ch);
        assert!(
            !dropped.load(Ordering::Acquire),
            "线程运行中 subscriber 不应 Drop"
        );
        handle.stop();
        handle.stop(); // 幂等
        handle.join().expect("join ok");
        assert!(
            dropped.load(Ordering::Acquire),
            "join 返回后 subscriber 必已正常 Drop（AC-E20：禁 mem::forget）"
        );
    }

    #[test]
    fn drop_handle_without_join_stops_and_joins_thread() {
        let ch = MockChannel::<markers::JointState>::new();
        let (handle, dropped) = spawn_probe(&ch);
        drop(handle);
        // Drop = stop + join：返回时线程已退出、subscriber 已析构。
        assert!(
            dropped.load(Ordering::Acquire),
            "Drop 路径必须 stop + join（best-effort）并让 subscriber 正常析构"
        );
    }

    // ----- 回滚路径 -----

    #[test]
    fn factory_error_returns_err_and_joins_thread() {
        let exited = Arc::new(AtomicBool::new(false));
        let exited_for_factory = Arc::clone(&exited);
        let result = spawn_state_cache_thread_with(
            cfg(),
            Arc::new(SystemClock::new()),
            READY_TIMEOUT,
            move || -> Result<MockSubscriber<markers::JointState>, StateCacheSpawnError> {
                exited_for_factory.store(true, Ordering::Release);
                Err(StateCacheSpawnError::SubscriberOpen(
                    TransportError::Backend("simulated open failure".into()),
                ))
            },
        );
        let err = result.expect_err("expected Err");
        assert!(
            matches!(err, StateCacheSpawnError::SubscriberOpen(TransportError::Backend(ref s))
                if s == "simulated open failure"),
            "got {err:?}"
        );
        assert!(exited.load(Ordering::Acquire));
    }

    #[test]
    fn ready_timeout_rolls_back_and_joins_slow_factory() {
        let factory_done = Arc::new(AtomicBool::new(false));
        let done_for_factory = Arc::clone(&factory_done);
        let ch = MockChannel::<markers::JointState>::new();
        let wall = SystemClock::new();
        let start = wall.now();
        let result = spawn_state_cache_thread_with(
            cfg(),
            Arc::new(SystemClock::new()),
            Duration::from_millis(30),
            move || {
                // 比 ready_timeout 慢得多：主线程先超时，再等本 factory 完成
                // 后线程看到 stop 退出；spawn 必须 join 到那时才返回。
                thread::sleep(Duration::from_millis(150));
                done_for_factory.store(true, Ordering::Release);
                Ok(ch.subscriber())
            },
        );
        let err = result.expect_err("expected Err");
        assert!(
            matches!(err, StateCacheSpawnError::ReadyTimeout),
            "got {err:?}"
        );
        assert!(
            factory_done.load(Ordering::Acquire),
            "ReadyTimeout 路径必须 join 慢线程（不留孤儿）"
        );
        let elapsed_ns = wall.now().0.saturating_sub(start.0);
        assert!(
            elapsed_ns >= 150_000_000,
            "spawn 返回不得早于慢 factory 完成（join 证据）: elapsed_ns={elapsed_ns}"
        );
    }

    // ----- 类型映射 -----

    #[test]
    fn frame_check_error_maps_exhaustively() {
        let cases: Vec<(FrameCheckError, StaleOrMissing)> = vec![
            (
                FrameCheckError::Decode("x".into()),
                StaleOrMissing::Decode("x".into()),
            ),
            (
                FrameCheckError::LayoutMismatch {
                    expected: 1,
                    got: 2,
                },
                StaleOrMissing::LayoutMismatch {
                    expected: 1,
                    got: 2,
                },
            ),
            (
                FrameCheckError::Stale {
                    stamp_ns: 1,
                    now_ns: 2,
                    max_stale_ns: 3,
                },
                StaleOrMissing::Stale {
                    stamp_ns: 1,
                    now_ns: 2,
                    max_stale_ns: 3,
                },
            ),
            (
                FrameCheckError::MissingPositionField,
                StaleOrMissing::MissingPositionField,
            ),
            (
                FrameCheckError::JointCountMismatch {
                    expected: 3,
                    got: 2,
                },
                StaleOrMissing::JointCountMismatch {
                    expected: 3,
                    got: 2,
                },
            ),
            (
                FrameCheckError::NonFinitePosition { index: 7 },
                StaleOrMissing::NonFinitePosition { index: 7 },
            ),
        ];
        for (from, want) in cases {
            assert_eq!(StaleOrMissing::from(from), want);
        }
    }

    #[test]
    fn stale_or_missing_display_is_human_readable() {
        assert!(StaleOrMissing::NoFrame.to_string().contains("no frame"));
        let s = StaleOrMissing::LayoutMismatch {
            expected: 0xAB,
            got: 0xCD,
        }
        .to_string();
        assert!(
            s.contains("0x00000000000000ab") && s.contains("0x00000000000000cd"),
            "{s}"
        );
    }

    #[test]
    fn config_max_stale_ns_saturates() {
        let mut c = cfg();
        c.max_stale_ms = u64::MAX;
        assert_eq!(c.max_stale_ns(), u64::MAX);
        c.max_stale_ms = 200;
        assert_eq!(c.max_stale_ns(), 200_000_000);
    }
}
