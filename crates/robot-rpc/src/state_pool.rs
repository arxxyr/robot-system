//! `robot-rpc::state_pool` —— v0.5 候选 B (2/4)：gateway 常驻 per-hardware S0
//! state 订阅池 + latest-state cache。
//!
//! ## 角色（D4 常驻订阅池）
//!
//! gateway 启动时按 `--config` 声明的每个 `[[hardware]].id`（D10）建一条
//! **常驻** iceoryx2 subscriber 线程，订阅 `/<robot_id>/s0/<hardware_id>/state`
//! 上 S0 发布的 fbs `JointState`，循环读 → 把最新一帧的 **owned body bytes**
//! 写进 [`StateCache`]（keep-last）。
//!
//! - **唯一底层 subscriber 来源**（reviewer Plan round 1 锁点 2）：`GetState`
//!   ((2/4)) 与 `StreamState` ((3/4)) 都**只读 cache**，**不**为每 RPC 新建
//!   底层 iceoryx2 subscriber。
//! - **convert-on-read（省高频转换）**：cache 接管 body 缓冲并用 `Arc` 共享，
//!   读快照只增加引用计数，解锁后才 root + convert，不复制整帧。
//! - **错误码三态**（D9）：cache 区分"未知 hardware"（key 不在池 → NotFound）
//!   / "已知但无首帧"（key 在但值 None → Unavailable）/ "有首帧"（Frame → Ok）。
//!
//! ## 生命周期
//!
//! [`StatePool::spawn`] 在 gateway 启动时建；[`StatePool::stop`] 在 tonic
//! server 退出**之后**调（沿 B4 shutdown 顺序，与 5 个 rr_io_thread 一并
//! stop+join）。
//!
//! ## 线程模型（沿 `tests/common/fake_state.rs` + s0 daemon 先例）
//!
//! 每 hardware 一条 std::thread，**线程内** create iceoryx2 Node + subscriber
//! （iceoryx2 句柄 !Send，必须 owner 线程内 create + `Box::leak` Node 保活）。
//! `stop` AtomicBool 触发退出，subscriber 正常 Drop（0.8.1 时代此处曾
//! `mem::forget` 规避 publisher 先退 → Drop 访问已释放 shm SIGSEGV；iceoryx2
//! 0.9.3〔上游 #1629 同根因修复〕升级后撤销，estop_e2e / state_rpc_e2e 复现无
//! SIGSEGV）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use iceoryx2::prelude::NodeBuilder;
use iceoryx2::service::ipc::Service as IxService;
use robot_msg::markers;
use robot_transport::iceoryx2::{TopicClass, subscriber as open_sub};
use robot_transport::traits::LatestSubscriber;

/// subscriber 线程 poll 间隔。keep-last 语义下只取最新帧，2ms（500Hz）poll
/// 远高于 S0 publish rate，cache 至多 2ms 陈旧（vs StreamState 默认 50ms tick
/// 可忽略），CPU 占用低。
const POLL_INTERVAL: Duration = Duration::from_millis(2);

/// `StateCache::snapshot` 结果——直接对应 D9 三态错误码口径。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateLookup {
    /// `hardware_id` 不在 `--config` 声明的 hardware 池 → `NotFound`（禁
    /// fallback，AC-9）。
    UnknownHardware,
    /// `hardware_id` 已知但常驻订阅尚未拿到首帧 → `Unavailable`。
    NoFrame,
    /// 有首帧——共享的只读 body；缓存换帧不影响已取出的快照。
    Frame(Arc<Vec<u8>>),
}

/// per-hardware latest-state cache。key 集在 [`StateCache::new`] 时按 config
/// 固定（之后不增删 key）；每 value 是 keep-last owned body bytes 的内部可变槽。
#[derive(Debug, Default)]
pub struct StateCache {
    per_hw: HashMap<String, Mutex<Option<Arc<Vec<u8>>>>>,
}

impl StateCache {
    /// 按 config 声明的 hardware id 列表建固定 key 集（每 key 初值 = 无首帧）。
    #[must_use]
    pub fn new(hardware_ids: &[String]) -> Self {
        let per_hw = hardware_ids
            .iter()
            .map(|id| (id.clone(), Mutex::new(None)))
            .collect();
        Self { per_hw }
    }

    /// subscriber 线程调：把某 hardware 的最新 body bytes 写进 cache（keep-last
    /// 覆盖）。`hardware_id` 不在池则静默忽略（不会发生——线程只为池内 id 建）。
    pub fn store(&self, hardware_id: &str, body_bytes: Vec<u8>) {
        if let Some(slot) = self.per_hw.get(hardware_id) {
            // Arc 包住原 Vec，保留现有字节缓冲；只在入库时分配引用计数对象。
            let frame = Arc::new(body_bytes);
            let mut guard = slot.lock().unwrap_or_else(PoisonError::into_inner);
            *guard = Some(frame);
        }
    }

    /// `GetState` / `StreamState` 读路径调：按 D9 三态返回。
    #[must_use]
    pub fn snapshot(&self, hardware_id: &str) -> StateLookup {
        match self.per_hw.get(hardware_id) {
            None => StateLookup::UnknownHardware,
            Some(slot) => {
                let guard = slot.lock().unwrap_or_else(PoisonError::into_inner);
                match &*guard {
                    Some(bytes) => StateLookup::Frame(Arc::clone(bytes)),
                    None => StateLookup::NoFrame,
                }
            }
        }
    }
}

/// 单条 state pool 线程的 liveness 面（v0.5 候选 E 段(8)；R6-5：**per-hardware
/// 一线程各一 observer**，禁聚合求和——健康线程推进会掩蔽卡死线程）。
#[derive(Debug)]
pub struct StatePoolThreadMetrics {
    /// 该线程订阅的 hardware id（observer label 用）。
    pub hardware_id: String,
    /// 每次 poll 迭代 +1（空闲亦推进）——watchdog liveness 源。
    pub loop_tick_count: AtomicU64,
    /// test-only：loop 顶端 freeze 截止（absolute ns，0 = 未冻结）。
    #[cfg(any(test, feature = "test-freeze-hook"))]
    pub freeze_until_ns: AtomicU64,
}

impl StatePoolThreadMetrics {
    /// 新建（计数全 0）。
    #[must_use]
    pub fn new(hardware_id: &str) -> Self {
        Self {
            hardware_id: hardware_id.to_string(),
            loop_tick_count: AtomicU64::new(0),
            #[cfg(any(test, feature = "test-freeze-hook"))]
            freeze_until_ns: AtomicU64::new(0),
        }
    }
}

/// 常驻 per-hardware state 订阅池 handle。owner = [`crate::gateway`]；shutdown
/// 时 [`StatePool::stop`] 一键 stop+join 全部 subscriber 线程。
pub struct StatePool {
    handles: Vec<Option<JoinHandle<()>>>,
    joined: Vec<std::thread::Result<()>>,
    stop: Arc<AtomicBool>,
    /// 顺序 = `hardware_ids` 顺序（`WorkerKind::StatePool(index)` 索引域）。
    metrics: Vec<Arc<StatePoolThreadMetrics>>,
}

pub struct StatePoolSpawnRejected {
    pub partial: StatePool,
    pub error: std::io::Error,
}
impl std::fmt::Debug for StatePoolSpawnRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StatePoolSpawnRejected")
            .field("error", &self.error)
            .field("spawned", &self.partial.handles.len())
            .finish_non_exhaustive()
    }
}

impl StatePool {
    /// 按 `hardware_ids` 起 N 条常驻 subscriber 线程，写进 `cache`。
    ///
    /// 每条线程 ready（subscriber open 成功）后才登记；任一 hardware 的
    /// subscriber open 失败 → stop+join 已起线程 → 返 `Err`（gateway 启动
    /// fail-fast，沿 rr_io_thread spawn 回滚先例）。`hardware_ids` 空 → 返
    /// 空池（合法，D-P1：`--config` 无 hardware 等价无 state 池）。
    ///
    /// # Errors
    ///
    /// - 某 hardware subscriber open 失败（iceoryx2 service create / 配额等）。
    /// - 线程 spawn 失败（罕见）。
    pub fn spawn(
        robot_id: &str,
        hardware_ids: &[String],
        cache: &Arc<StateCache>,
    ) -> Result<Self, StatePoolSpawnRejected> {
        let stop = Arc::new(AtomicBool::new(false));
        let mut handles: Vec<Option<JoinHandle<()>>> = Vec::with_capacity(hardware_ids.len());
        let mut metrics: Vec<Arc<StatePoolThreadMetrics>> = Vec::with_capacity(hardware_ids.len());

        for hw in hardware_ids {
            let topic = format!("/{robot_id}/s0/{hw}/state");
            let topic_for_thread = topic.clone();
            let hw_owned = hw.clone();
            let stop_clone = Arc::clone(&stop);
            let cache_clone = Arc::clone(cache);
            let thread_metrics = Arc::new(StatePoolThreadMetrics::new(hw));
            let metrics_clone = Arc::clone(&thread_metrics);
            let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<bool>(0);

            let handle = thread::Builder::new()
                .name(format!("state_pool_{hw}"))
                .spawn(move || {
                    let node = match NodeBuilder::new().create::<IxService>() {
                        Ok(n) => n,
                        Err(e) => {
                            eprintln!("[state_pool] NodeBuilder create failed ({hw_owned}): {e:?}");
                            let _ = ready_tx.send(false);
                            return;
                        }
                    };
                    let mut sub = match open_sub::<markers::JointState>(
                        &node,
                        &topic_for_thread,
                        TopicClass::S0State,
                    ) {
                        Ok(s) => s,
                        Err(e) => {
                            eprintln!("[state_pool] open_sub failed for {topic_for_thread}: {e:?}");
                            let _ = ready_tx.send(false);
                            return;
                        }
                    };
                    let _ = ready_tx.send(true);
                    drop(ready_tx);

                    while !stop_clone.load(Ordering::Acquire) {
                        // v0.5 候选 E 段(8)：liveness 计数（空闲亦推进）+ test-only
                        // freeze hook（per-hardware 线程各自槽位）。
                        metrics_clone
                            .loop_tick_count
                            .fetch_add(1, Ordering::Relaxed);
                        #[cfg(any(test, feature = "test-freeze-hook"))]
                        crate::liveness::freeze_check(
                            &metrics_clone.freeze_until_ns,
                            &robot_core::time::SystemClock::new(),
                            &stop_clone,
                        );
                        // 有新帧 → 拷贝 raw body bytes（= FrameHeader.body_size）进
                        // cache（读路径再 root + convert，convert-on-read）；无新帧 /
                        // 读错误 → 不进 if → 保留旧 cache（keep-last 持久）。
                        if let Ok(Some(lease)) = sub.latest()
                            && let Ok(view) = lease.view()
                        {
                            cache_clone.store(&hw_owned, view.body.raw().to_vec());
                        }
                        thread::sleep(POLL_INTERVAL);
                    }
                });
            let handle = match handle {
                Ok(handle) => handle,
                Err(error) => {
                    return Err(StatePoolSpawnRejected {
                        partial: Self {
                            handles,
                            stop,
                            metrics,
                            joined: Vec::new(),
                        },
                        error,
                    });
                }
            };
            handles.push(Some(handle));
            metrics.push(thread_metrics);
            if !matches!(ready_rx.recv(), Ok(true)) {
                return Err(StatePoolSpawnRejected {
                    partial: Self {
                        handles,
                        stop,
                        metrics,
                        joined: Vec::new(),
                    },
                    error: std::io::Error::other(format!(
                        "state_pool: failed to open subscriber for hardware '{hw}' (topic {topic})"
                    )),
                });
            }
        }

        Ok(Self {
            handles,
            stop,
            metrics,
            joined: Vec::new(),
        })
    }

    /// 每条 subscriber 线程的 liveness 面（顺序 = spawn 时 `hardware_ids` 顺序）。
    #[must_use]
    pub fn thread_metrics(&self) -> &[Arc<StatePoolThreadMetrics>] {
        &self.metrics
    }

    /// 信号 stop + join 全部 subscriber 线程。tonic server 退出**之后**调
    /// （沿 B4 shutdown 顺序）。
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        for slot in &mut self.handles {
            if let Some(handle) = slot.take() {
                self.joined.push(handle.join());
            }
        }
    }
    pub(crate) fn joined_ok(&self) -> bool {
        self.handles.iter().all(Option::is_none) && self.joined.iter().all(Result::is_ok)
    }
    pub(crate) fn join_failed(&self) -> bool {
        self.joined.iter().any(Result::is_err)
    }
}

// ============================================================================
// Tests（StateCache 纯逻辑；StatePool 线程 + iceoryx2 由 (4/4) cross-process
// e2e 覆盖）
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_hardware_when_not_in_pool() {
        let cache = StateCache::new(&["arm".to_string()]);
        assert_eq!(cache.snapshot("base"), StateLookup::UnknownHardware);
    }

    #[test]
    fn no_frame_before_first_store() {
        let cache = StateCache::new(&["arm".to_string()]);
        assert_eq!(cache.snapshot("arm"), StateLookup::NoFrame);
    }

    #[test]
    fn frame_after_store() {
        let cache = StateCache::new(&["arm".to_string()]);
        cache.store("arm", vec![1, 2, 3]);
        assert_eq!(
            cache.snapshot("arm"),
            StateLookup::Frame(Arc::new(vec![1, 2, 3]))
        );
    }

    #[test]
    fn snapshots_share_original_buffer_and_outlive_cache_replacement() {
        let cache = StateCache::new(&["arm".to_string()]);
        let mut body = Vec::with_capacity(128);
        body.extend_from_slice(&[1, 2, 3]);
        let allocation = (body.as_ptr(), body.capacity());
        cache.store("arm", body);
        let StateLookup::Frame(first) = cache.snapshot("arm") else {
            panic!("应有首帧")
        };
        let StateLookup::Frame(second) = cache.snapshot("arm") else {
            panic!("应有首帧")
        };
        assert_eq!((first.as_ptr(), first.capacity()), allocation);
        assert!(Arc::ptr_eq(&first, &second));

        let old_frame = Arc::downgrade(&first);
        cache.store("arm", vec![4, 5]);
        let StateLookup::Frame(latest) = cache.snapshot("arm") else {
            panic!("应有新帧")
        };
        assert!(!Arc::ptr_eq(&first, &latest));
        assert_eq!(latest.as_slice(), [4, 5]);
        drop(cache);
        assert_eq!(first.as_slice(), [1, 2, 3]);
        assert_eq!(second.as_slice(), [1, 2, 3]);
        assert_eq!(latest.as_slice(), [4, 5]);
        drop(first);
        drop(second);
        assert!(old_frame.upgrade().is_none(), "最后一个读者退出后释放旧帧");
    }

    #[test]
    fn store_keeps_last() {
        let cache = StateCache::new(&["arm".to_string()]);
        cache.store("arm", vec![1]);
        cache.store("arm", vec![2, 2]);
        assert_eq!(
            cache.snapshot("arm"),
            StateLookup::Frame(Arc::new(vec![2, 2]))
        );
    }

    #[test]
    fn store_unknown_hardware_is_noop() {
        let cache = StateCache::new(&["arm".to_string()]);
        cache.store("base", vec![9]);
        assert_eq!(cache.snapshot("base"), StateLookup::UnknownHardware);
        assert_eq!(cache.snapshot("arm"), StateLookup::NoFrame);
    }

    #[test]
    fn empty_pool_all_unknown() {
        let cache = StateCache::new(&[]);
        assert_eq!(cache.snapshot("anything"), StateLookup::UnknownHardware);
    }

    #[test]
    fn multi_hardware_isolated() {
        let cache = StateCache::new(&["arm".to_string(), "base".to_string()]);
        cache.store("arm", vec![7]);
        assert_eq!(cache.snapshot("arm"), StateLookup::Frame(Arc::new(vec![7])));
        assert_eq!(cache.snapshot("base"), StateLookup::NoFrame);
    }
}
