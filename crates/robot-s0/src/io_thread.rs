//! `s0_io_thread`：在独立 `SoftRT` 线程中轮询相机，向 Pub 提供容量为一的最新帧。
//! 相机错误只清空图像并回收采集进程；执行器与 IPC/Pub 继续独立推进。

#![forbid(unused_must_use)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use robot_core::adapter::SensorAdapter;
use robot_core::error::RobotError;
use robot_core::time::{Deadline, MonoRawNs, RobotClock};

use crate::boot_supervisor::BootShared;
use crate::camera::CameraFrame;

pub const CAMERA_MAX_AGE_NS: u64 = 1_000_000_000;
pub type CameraAdapter = Box<dyn SensorAdapter<Frame = CameraFrame>>;

/// 一路相机及其独立的最新帧槽。
pub struct CameraInput {
    pub adapter: CameraAdapter,
    pub frames: Arc<CameraFrames>,
}

/// 跨 SoftRT/Pub 的最新帧槽；锁内只交换所有权，不复制或释放像素。
#[derive(Debug, Default)]
pub struct CameraFrames {
    latest: Mutex<Option<CameraFrame>>,
    active: AtomicBool,
}

impl CameraFrames {
    /// 返回是否替换了尚未发布的帧；旧帧在解锁后释放。
    pub fn replace(&self, frame: CameraFrame) -> bool {
        let old = {
            self.latest
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .replace(frame)
        };
        self.active.store(true, Ordering::Release);
        old.is_some()
    }

    /// Pub 不等待采集锁；获取后全部像素处理在锁外完成。
    pub fn try_take(&self) -> Option<CameraFrame> {
        if !self.is_active() {
            return None;
        }
        self.latest.try_lock().ok()?.take()
    }

    #[must_use]
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    /// 先使帧失效，再取走旧帧于锁外释放。
    pub fn clear(&self) {
        self.active.store(false, Ordering::Release);
        let old = {
            self.latest
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
        };
        drop(old);
    }
}

pub(crate) fn camera_frame_valid(frame: &CameraFrame, now_ns: u64) -> bool {
    frame.width > 0
        && frame.height > 0
        && frame.width.checked_mul(3) == Some(frame.step)
        && frame
            .step
            .checked_mul(frame.height)
            .and_then(|size| usize::try_from(size).ok())
            == Some(frame.data.len())
        && frame.data.len() <= robot_msg::size_tier::SizeTier::SensorLarge.max_bytes() - 4096
        && frame.stamp_ns <= now_ns
        && now_ns - frame.stamp_ns <= CAMERA_MAX_AGE_NS
}

#[derive(Clone, Copy, Debug)]
pub struct IoThreadConfig {
    /// poll 周期（纳秒）。默认 10 ms（100 Hz）。
    pub poll_interval_ns: u64,
}

impl Default for IoThreadConfig {
    fn default() -> Self {
        Self {
            poll_interval_ns: 10_000_000,
        }
    }
}

#[derive(Debug, Default)]
pub struct IoThreadMetrics {
    pub tick_count: AtomicU64,
    /// 无相机时立即为真；有相机时须全部路均已产首帧且仍活跃。
    pub ready: AtomicBool,
    pub startup_failed: AtomicBool,
    pub camera_errors: AtomicU64,
    pub frames: AtomicU64,
    pub dropped: AtomicU64,
    /// 所有路已接受帧的最大时间戳，仅用于观测。
    pub last_frame_ns: AtomicU64,
    /// stop/reap 失败为粘性事实，同时由 join 的 Err 交给原回收路径。
    pub cleanup_failed: AtomicBool,
    #[cfg(any(test, feature = "test-freeze-hook"))]
    pub freeze_until_ns: AtomicU64,
}

pub struct IoThreadHandle {
    join: JoinHandle<Result<(), RobotError>>,
    stop: Arc<AtomicBool>,
    metrics: Arc<IoThreadMetrics>,
}

impl IoThreadHandle {
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
    }
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.join.is_finished()
    }
    #[must_use]
    pub fn metrics(&self) -> &Arc<IoThreadMetrics> {
        &self.metrics
    }

    /// 保留原 `WorkerSlot` 的 join 合同；相机清理错误以 `RobotError` payload 返回 Err。
    /// # Errors
    /// 线程 panic，或 stop/reap 失败（可 downcast 为 `RobotError`）。
    pub fn join(self) -> thread::Result<()> {
        match self.join.join() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(Box::new(error)),
            Err(panic) => Err(panic),
        }
    }
}

/// 保留未配置相机时的原入口。
pub fn spawn_io_thread(
    cfg: IoThreadConfig,
    clock: Arc<dyn RobotClock>,
    stop: Arc<AtomicBool>,
) -> std::io::Result<IoThreadHandle> {
    spawn_io_thread_with_cameras(cfg, clock, stop, Vec::new(), None)
}

pub fn spawn_io_thread_with_cameras(
    cfg: IoThreadConfig,
    clock: Arc<dyn RobotClock>,
    stop: Arc<AtomicBool>,
    cameras: Vec<CameraInput>,
    boot: Option<Arc<BootShared>>,
) -> std::io::Result<IoThreadHandle> {
    #[cfg(feature = "test-comm-fault-hook")]
    crate::comm_fault_hook::before_spawn("io")?;
    let metrics = Arc::new(IoThreadMetrics::default());
    let join_stop = Arc::clone(&stop);
    let join_metrics = Arc::clone(&metrics);
    let join = thread::Builder::new()
        .name("s0_io_thread".into())
        .spawn(move || {
            run_loop(
                cfg,
                clock.as_ref(),
                &join_stop,
                &join_metrics,
                cameras,
                boot.as_deref(),
            )
        })?;
    Ok(IoThreadHandle {
        join,
        stop,
        metrics,
    })
}

struct CameraState {
    adapter: Option<CameraAdapter>,
    frames: Arc<CameraFrames>,
    last_frame_ns: Option<u64>,
}

fn stop_camera(camera: &mut CameraState, metrics: &IoThreadMetrics) -> Result<(), RobotError> {
    camera.frames.clear();
    let Some(mut adapter) = camera.adapter.take() else {
        return Ok(());
    };
    let result = adapter.stop();
    if result.is_err() {
        metrics.cleanup_failed.store(true, Ordering::Release);
    }
    result
}

fn camera_fault(
    camera: &mut CameraState,
    metrics: &IoThreadMetrics,
    boot: Option<&BootShared>,
) -> Result<(), RobotError> {
    metrics.camera_errors.fetch_add(1, Ordering::Relaxed);
    metrics.ready.store(false, Ordering::Release);
    if camera.last_frame_ns.is_none() || boot.is_some_and(|boot| !boot.ready_reached()) {
        metrics.startup_failed.store(true, Ordering::Release);
    }
    stop_camera(camera, metrics)
}

fn run_loop(
    cfg: IoThreadConfig,
    clock: &dyn RobotClock,
    stop: &AtomicBool,
    metrics: &IoThreadMetrics,
    cameras: Vec<CameraInput>,
    boot: Option<&BootShared>,
) -> Result<(), RobotError> {
    let mut cameras: Vec<_> = cameras
        .into_iter()
        .map(|input| CameraState {
            adapter: Some(input.adapter),
            frames: input.frames,
            last_frame_ns: None,
        })
        .collect();
    let mut cleanup_result = Ok(());
    for (camera_index, camera) in cameras.iter_mut().enumerate() {
        let Some(adapter) = camera.adapter.as_mut() else {
            continue;
        };
        if let Err(error) = adapter.start() {
            tracing::error!(camera_index, ?error, "相机启动失败");
            cleanup_result = cleanup_result.and(camera_fault(camera, metrics, boot));
        }
    }
    metrics.ready.store(cameras.is_empty(), Ordering::Release);
    while crate::worker_stop::worker_running(crate::worker_stop::WorkerKind::Io, clock, stop) {
        #[cfg(any(test, feature = "test-freeze-hook"))]
        crate::freeze_hook::freeze_check(&metrics.freeze_until_ns, clock, stop);
        for (camera_index, camera) in cameras.iter_mut().enumerate() {
            let Some(adapter) = camera.adapter.as_mut() else {
                continue;
            };
            let deadline = Deadline(MonoRawNs(clock.now().0.saturating_add(1_000_000)));
            match adapter.poll_frame(deadline) {
                Ok(Some(frame))
                    if camera_frame_valid(&frame, clock.now().0)
                        && camera
                            .last_frame_ns
                            .is_none_or(|last| frame.stamp_ns > last) =>
                {
                    camera.last_frame_ns = Some(frame.stamp_ns);
                    metrics
                        .last_frame_ns
                        .fetch_max(frame.stamp_ns, Ordering::Relaxed);
                    if camera.frames.replace(frame) {
                        metrics.dropped.fetch_add(1, Ordering::Relaxed);
                    }
                    metrics.frames.fetch_add(1, Ordering::Relaxed);
                }
                Ok(Some(_)) => {
                    camera.frames.clear();
                    metrics.dropped.fetch_add(1, Ordering::Relaxed);
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::error!(camera_index, ?error, "相机采集失败，停用并回收相机");
                    cleanup_result = cleanup_result.and(camera_fault(camera, metrics, boot));
                }
            }
            // 全局 READY 前沿用原启动预算；保留最后时间戳，READY 后不重给失流宽限。
            let now_ns = clock.now().0;
            if camera.adapter.is_some()
                && let Some(last_frame_ns) = camera.last_frame_ns
                && now_ns.saturating_sub(last_frame_ns) > CAMERA_MAX_AGE_NS
            {
                if boot.is_some_and(|boot| !boot.ready_reached()) {
                    if camera.frames.is_active() {
                        tracing::warn!(
                            camera_index,
                            last_frame_ns,
                            age_ns = now_ns.saturating_sub(last_frame_ns),
                            "相机启动期超过新鲜度期限，等待新帧"
                        );
                    }
                    camera.frames.clear();
                    metrics.ready.store(false, Ordering::Release);
                } else {
                    tracing::error!(camera_index, "相机超过新鲜度期限未产新帧，停用并回收相机");
                    cleanup_result = cleanup_result.and(camera_fault(camera, metrics, boot));
                }
            }
        }
        metrics.ready.store(
            cameras.iter().all(|camera| {
                camera.adapter.is_some()
                    && camera.last_frame_ns.is_some()
                    && camera.frames.is_active()
            }),
            Ordering::Release,
        );
        metrics.tick_count.fetch_add(1, Ordering::Relaxed);
        thread::sleep(Duration::from_nanos(cfg.poll_interval_ns));
    }
    for camera in &mut cameras {
        cleanup_result = cleanup_result.and(stop_camera(camera, metrics));
    }
    metrics.ready.store(cameras.is_empty(), Ordering::Release);
    cleanup_result
}

#[cfg(test)]
mod tests {
    use super::*;
    use robot_core::time::{MockClock, SystemClock};

    fn test_clock() -> Arc<dyn RobotClock> {
        Arc::new(SystemClock::new())
    }

    fn frame(stamp_ns: u64, byte: u8) -> CameraFrame {
        CameraFrame {
            width: 2,
            height: 1,
            step: 6,
            stamp_ns,
            data: vec![byte; 6],
        }
    }

    struct FakeCamera {
        results: std::collections::VecDeque<Result<Option<CameraFrame>, RobotError>>,
        start_failed: bool,
        stop_failed: bool,
        stops: Arc<AtomicU64>,
    }

    impl SensorAdapter for FakeCamera {
        type Frame = CameraFrame;
        fn start(&mut self) -> Result<(), RobotError> {
            if self.start_failed {
                Err(RobotError::HardwareFault("模拟启动失败"))
            } else {
                Ok(())
            }
        }
        fn poll_frame(&mut self, _deadline: Deadline) -> Result<Option<CameraFrame>, RobotError> {
            self.results.pop_front().unwrap_or(Ok(None))
        }
        fn stop(&mut self) -> Result<(), RobotError> {
            self.stops.fetch_add(1, Ordering::Relaxed);
            if self.stop_failed {
                Err(RobotError::HardwareFault("模拟回收失败"))
            } else {
                Ok(())
            }
        }
    }

    type PollResults =
        Arc<Mutex<std::collections::VecDeque<Result<Option<CameraFrame>, RobotError>>>>;

    #[derive(Default)]
    struct CameraFixture {
        results: PollResults,
        frames: Arc<CameraFrames>,
        stops: Arc<AtomicU64>,
    }

    impl CameraFixture {
        fn push(&self, result: Result<Option<CameraFrame>, RobotError>) {
            self.results.lock().unwrap().push_back(result);
        }

        fn input(&self, stop_error: Option<RobotError>) -> CameraInput {
            CameraInput {
                adapter: Box::new(QueuedCamera {
                    results: Arc::clone(&self.results),
                    stops: Arc::clone(&self.stops),
                    stop_error,
                }),
                frames: Arc::clone(&self.frames),
            }
        }
    }

    struct QueuedCamera {
        results: PollResults,
        stops: Arc<AtomicU64>,
        stop_error: Option<RobotError>,
    }

    impl SensorAdapter for QueuedCamera {
        type Frame = CameraFrame;

        fn start(&mut self) -> Result<(), RobotError> {
            Ok(())
        }

        fn poll_frame(&mut self, _deadline: Deadline) -> Result<Option<CameraFrame>, RobotError> {
            self.results.lock().unwrap().pop_front().unwrap_or(Ok(None))
        }

        fn stop(&mut self) -> Result<(), RobotError> {
            self.stops.fetch_add(1, Ordering::Relaxed);
            self.stop_error.map_or(Ok(()), Err)
        }
    }

    fn spawn_cameras(clock: Arc<dyn RobotClock>, cameras: Vec<CameraInput>) -> IoThreadHandle {
        spawn_cameras_with_boot(clock, cameras, None)
    }

    fn spawn_cameras_with_boot(
        clock: Arc<dyn RobotClock>,
        cameras: Vec<CameraInput>,
        boot: Option<Arc<BootShared>>,
    ) -> IoThreadHandle {
        spawn_io_thread_with_cameras(
            IoThreadConfig {
                poll_interval_ns: 1_000_000,
            },
            clock,
            Arc::new(AtomicBool::new(false)),
            cameras,
            boot,
        )
        .unwrap()
    }

    fn install_runtime(boot: &BootShared) -> crate::boot_supervisor::ReadyPermit<'_> {
        boot.install_runtime_for_test(
            crate::hard_rt_ctrl::test_authority(false),
            Arc::new(crate::latch_writer::PersistShared::new(boot.started_at()).unwrap()),
        )
        .unwrap_or_else(|_| panic!("相机测试安装真实运行期绑定"))
    }

    #[test]
    fn camera_latest_replaces_and_clear_discards_without_blocking_reader() {
        let frames = CameraFrames::default();
        assert!(!frames.replace(frame(1, 1)));
        assert!(frames.replace(frame(2, 2)));
        let guard = frames.latest.lock().unwrap();
        assert!(frames.try_take().is_none());
        drop(guard);
        assert_eq!(frames.try_take().unwrap().data, [2; 6]);
        assert!(frames.try_take().is_none());
        frames.replace(frame(3, 3));
        frames.clear();
        assert!(frames.try_take().is_none());
        assert!(!frames.is_active());
    }

    #[test]
    fn camera_validity_rejects_old_future_and_malformed_frames() {
        assert!(camera_frame_valid(&frame(0, 0), CAMERA_MAX_AGE_NS));
        assert!(!camera_frame_valid(&frame(0, 0), CAMERA_MAX_AGE_NS + 1));
        assert!(!camera_frame_valid(&frame(2, 0), 1));
        let mut malformed = frame(1, 0);
        malformed.step = 5;
        assert!(!camera_frame_valid(&malformed, 1));
        malformed.step = 6;
        malformed.data.pop();
        assert!(!camera_frame_valid(&malformed, 1));
    }

    #[test]
    fn camera_runtime_fault_clears_frame_stops_once_and_io_keeps_ticking() {
        let clock = test_clock();
        let stops = Arc::new(AtomicU64::new(0));
        let camera = FakeCamera {
            results: [
                Ok(Some(frame(clock.now().0, 1))),
                Err(RobotError::HardwareFault("模拟采集失败")),
            ]
            .into(),
            start_failed: false,
            stop_failed: false,
            stops: Arc::clone(&stops),
        };
        let frames = Arc::new(CameraFrames::default());
        let handle = spawn_io_thread_with_cameras(
            IoThreadConfig {
                poll_interval_ns: 1_000_000,
            },
            clock,
            Arc::new(AtomicBool::new(false)),
            vec![CameraInput {
                adapter: Box::new(camera),
                frames: Arc::clone(&frames),
            }],
            None,
        )
        .unwrap();
        assert!(crate::test_util::wait_until(
            Duration::from_secs(2),
            || handle.metrics().camera_errors.load(Ordering::Relaxed) == 1
        ));
        let ticks = handle.metrics().tick_count.load(Ordering::Relaxed);
        assert!(crate::test_util::wait_until(
            Duration::from_secs(2),
            || handle.metrics().tick_count.load(Ordering::Relaxed) > ticks
        ));
        assert!(!handle.metrics().startup_failed.load(Ordering::Acquire));
        assert_eq!(handle.metrics().frames.load(Ordering::Relaxed), 1);
        assert_eq!(stops.load(Ordering::Relaxed), 1);
        assert!(frames.try_take().is_none());
        handle.request_stop();
        handle.join().unwrap();
        assert_eq!(stops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn camera_without_first_frame_stays_unready_and_reaps_on_stop() {
        let stops = Arc::new(AtomicU64::new(0));
        let camera = FakeCamera {
            results: [].into(),
            start_failed: false,
            stop_failed: false,
            stops: Arc::clone(&stops),
        };
        let handle = spawn_io_thread_with_cameras(
            IoThreadConfig {
                poll_interval_ns: 1_000_000,
            },
            test_clock(),
            Arc::new(AtomicBool::new(false)),
            vec![CameraInput {
                adapter: Box::new(camera),
                frames: Arc::new(CameraFrames::default()),
            }],
            None,
        )
        .unwrap();
        assert!(crate::test_util::wait_until(
            Duration::from_secs(2),
            || handle.metrics().tick_count.load(Ordering::Relaxed) >= 2
        ));
        assert!(!handle.metrics().ready.load(Ordering::Acquire));
        assert!(!handle.metrics().startup_failed.load(Ordering::Acquire));
        handle.request_stop();
        handle.join().unwrap();
        assert_eq!(stops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn camera_missing_fresh_frames_after_ready_stops_once_and_io_keeps_ticking() {
        let clock = Arc::new(robot_core::time::MockClock::new(MonoRawNs(0)));
        let stops = Arc::new(AtomicU64::new(0));
        let camera = FakeCamera {
            results: [Ok(Some(frame(0, 1)))].into(),
            start_failed: false,
            stop_failed: false,
            stops: Arc::clone(&stops),
        };
        let frames = Arc::new(CameraFrames::default());
        let clock_input: Arc<dyn RobotClock> = clock.clone();
        let handle = spawn_io_thread_with_cameras(
            IoThreadConfig {
                poll_interval_ns: 1_000_000,
            },
            clock_input,
            Arc::new(AtomicBool::new(false)),
            vec![CameraInput {
                adapter: Box::new(camera),
                frames: Arc::clone(&frames),
            }],
            None,
        )
        .unwrap();
        assert!(crate::test_util::wait_until(Duration::from_secs(2), || {
            handle.metrics().ready.load(Ordering::Acquire)
        }));
        let ticks = handle.metrics().tick_count.load(Ordering::Relaxed);
        clock.set(MonoRawNs(CAMERA_MAX_AGE_NS));
        assert!(crate::test_util::wait_until(
            Duration::from_secs(2),
            || handle.metrics().tick_count.load(Ordering::Relaxed) > ticks
        ));
        assert!(handle.metrics().ready.load(Ordering::Acquire));
        assert_eq!(stops.load(Ordering::Relaxed), 0);
        clock.set(MonoRawNs(CAMERA_MAX_AGE_NS + 1));
        assert!(crate::test_util::wait_until(
            Duration::from_secs(2),
            || stops.load(Ordering::Relaxed) == 1
        ));
        assert!(!handle.metrics().ready.load(Ordering::Acquire));
        assert!(!handle.metrics().startup_failed.load(Ordering::Acquire));
        assert!(frames.try_take().is_none());
        let stopped_tick = handle.metrics().tick_count.load(Ordering::Relaxed);
        assert!(crate::test_util::wait_until(
            Duration::from_secs(2),
            || handle.metrics().tick_count.load(Ordering::Relaxed) > stopped_tick
        ));
        assert_eq!(handle.metrics().camera_errors.load(Ordering::Relaxed), 1);
        assert_eq!(handle.metrics().frames.load(Ordering::Relaxed), 1);
        handle.request_stop();
        handle.join().unwrap();
        assert_eq!(stops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn camera_startup_failure_and_cleanup_error_are_visible_to_original_join() {
        let stops = Arc::new(AtomicU64::new(0));
        let camera = FakeCamera {
            results: [].into(),
            start_failed: true,
            stop_failed: true,
            stops: Arc::clone(&stops),
        };
        let handle = spawn_io_thread_with_cameras(
            IoThreadConfig::default(),
            test_clock(),
            Arc::new(AtomicBool::new(false)),
            vec![CameraInput {
                adapter: Box::new(camera),
                frames: Arc::new(CameraFrames::default()),
            }],
            None,
        )
        .unwrap();
        assert!(crate::test_util::wait_until(Duration::from_secs(2), || {
            handle.metrics().cleanup_failed.load(Ordering::Acquire)
        }));
        assert!(handle.metrics().startup_failed.load(Ordering::Acquire));
        assert!(!handle.metrics().ready.load(Ordering::Acquire));
        handle.request_stop();
        let error = handle.join().unwrap_err();
        assert_eq!(
            error.downcast_ref::<RobotError>(),
            Some(&RobotError::HardwareFault("模拟回收失败"))
        );
        assert_eq!(stops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn startup_stale_camera_waits_for_fresh_frames_from_every_camera() {
        let clock = Arc::new(MockClock::new(MonoRawNs(0)));
        let boot = Arc::new(BootShared::new(clock.now()));
        let _permit = install_runtime(&boot);
        let first = CameraFixture::default();
        let second = CameraFixture::default();
        first.push(Ok(Some(frame(0, 1))));
        let handle = spawn_cameras_with_boot(
            clock.clone(),
            vec![first.input(None), second.input(None)],
            Some(Arc::clone(&boot)),
        );
        assert!(crate::test_util::wait_until(Duration::from_secs(2), || {
            first.frames.is_active()
        }));
        assert!(!handle.metrics().ready.load(Ordering::Acquire));

        clock.set(MonoRawNs(CAMERA_MAX_AGE_NS + 1));
        assert!(crate::test_util::wait_until(Duration::from_secs(2), || {
            !first.frames.is_active()
        }));
        assert!(first.frames.try_take().is_none());
        assert!(!handle.metrics().ready.load(Ordering::Acquire));
        assert_eq!(first.stops.load(Ordering::Relaxed), 0);
        assert_eq!(second.stops.load(Ordering::Relaxed), 0);

        // 另一相机的首帧不能使已超龄的一路重新就绪。
        second.push(Ok(Some(frame(clock.now().0, 2))));
        assert!(crate::test_util::wait_until(Duration::from_secs(2), || {
            second.frames.is_active()
        }));
        assert!(!handle.metrics().ready.load(Ordering::Acquire));
        assert!(!first.frames.is_active());
        assert_eq!(first.stops.load(Ordering::Relaxed), 0);

        first.push(Ok(Some(frame(clock.now().0, 3))));
        assert!(crate::test_util::wait_until(Duration::from_secs(2), || {
            handle.metrics().ready.load(Ordering::Acquire)
        }));
        assert_eq!(first.frames.try_take().unwrap().data, [3; 6]);
        assert_eq!(handle.metrics().frames.load(Ordering::Relaxed), 3);
        assert_eq!(handle.metrics().camera_errors.load(Ordering::Relaxed), 0);
        assert!(!handle.metrics().startup_failed.load(Ordering::Acquire));
        assert!(!boot.ready_reached());
        assert_eq!(first.stops.load(Ordering::Relaxed), 0);
        assert_eq!(second.stops.load(Ordering::Relaxed), 0);
        handle.request_stop();
        handle.join().unwrap();
        assert_eq!(first.stops.load(Ordering::Relaxed), 1);
        assert_eq!(second.stops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn startup_stale_camera_stops_after_global_ready_without_new_grace_period() {
        let clock = Arc::new(MockClock::new(MonoRawNs(0)));
        let boot = Arc::new(BootShared::new(clock.now()));
        let permit = install_runtime(&boot);
        let camera = CameraFixture::default();
        camera.push(Ok(Some(frame(0, 1))));
        let handle = spawn_cameras_with_boot(
            clock.clone(),
            vec![camera.input(None)],
            Some(Arc::clone(&boot)),
        );
        assert!(crate::test_util::wait_until(Duration::from_secs(2), || {
            handle.metrics().ready.load(Ordering::Acquire)
        }));
        assert!(!boot.ready_reached());

        clock.set(MonoRawNs(CAMERA_MAX_AGE_NS + 1));
        assert!(crate::test_util::wait_until(Duration::from_secs(2), || {
            !camera.frames.is_active() && !handle.metrics().ready.load(Ordering::Acquire)
        }));
        assert!(camera.frames.try_take().is_none());
        assert_eq!(camera.stops.load(Ordering::Relaxed), 0);
        assert_eq!(handle.metrics().camera_errors.load(Ordering::Relaxed), 0);

        // 不推进时钟或补新帧；既有 permit 发布 READY 后，下一轮仍按原帧时间停止。
        permit.publish_ready(clock.now());
        assert!(crate::test_util::wait_until(Duration::from_secs(2), || {
            camera.stops.load(Ordering::Relaxed) == 1
        }));
        let ticks = handle.metrics().tick_count.load(Ordering::Relaxed);
        assert!(crate::test_util::wait_until(Duration::from_secs(2), || {
            handle.metrics().tick_count.load(Ordering::Relaxed) > ticks
        }));
        assert_eq!(camera.stops.load(Ordering::Relaxed), 1);
        assert_eq!(handle.metrics().camera_errors.load(Ordering::Relaxed), 1);
        assert!(!handle.metrics().startup_failed.load(Ordering::Acquire));
        assert!(!handle.metrics().ready.load(Ordering::Acquire));
        handle.request_stop();
        handle.join().unwrap();
        assert_eq!(camera.stops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn real_camera_errors_before_global_ready_fail_startup_and_stop_once() {
        for (start_failed, first_frame_received) in [(true, false), (false, false), (false, true)] {
            let clock = Arc::new(MockClock::new(MonoRawNs(0)));
            let boot = Arc::new(BootShared::new(clock.now()));
            let _permit = install_runtime(&boot);
            let stops = Arc::new(AtomicU64::new(0));
            let frames = Arc::new(CameraFrames::default());
            let mut results = std::collections::VecDeque::new();
            if first_frame_received {
                results.push_back(Ok(Some(frame(0, 1))));
            }
            results.push_back(Err(RobotError::HardwareFault("模拟启动期采集失败")));
            let camera = FakeCamera {
                results,
                start_failed,
                stop_failed: false,
                stops: Arc::clone(&stops),
            };
            let handle = spawn_cameras_with_boot(
                clock,
                vec![CameraInput {
                    adapter: Box::new(camera),
                    frames: Arc::clone(&frames),
                }],
                Some(Arc::clone(&boot)),
            );
            assert!(crate::test_util::wait_until(Duration::from_secs(2), || {
                handle.metrics().tick_count.load(Ordering::Relaxed) >= 3
            }));
            assert!(!boot.ready_reached());
            assert!(handle.metrics().startup_failed.load(Ordering::Acquire));
            assert!(!handle.metrics().ready.load(Ordering::Acquire));
            assert_eq!(handle.metrics().camera_errors.load(Ordering::Relaxed), 1);
            assert_eq!(
                handle.metrics().frames.load(Ordering::Relaxed),
                u64::from(first_frame_received)
            );
            assert!(frames.try_take().is_none());
            assert!(!frames.is_active());
            assert_eq!(stops.load(Ordering::Relaxed), 1);
            handle.request_stop();
            handle.join().unwrap();
            assert_eq!(stops.load(Ordering::Relaxed), 1);
        }
    }

    #[test]
    fn cameras_require_every_first_frame_and_check_timestamps_independently() {
        let clock = Arc::new(MockClock::new(MonoRawNs(1_000)));
        let first = CameraFixture::default();
        let second = CameraFixture::default();
        first.push(Ok(Some(frame(900, 1))));
        let handle = spawn_cameras(clock, vec![first.input(None), second.input(None)]);
        assert!(crate::test_util::wait_until(Duration::from_secs(2), || {
            handle.metrics().tick_count.load(Ordering::Relaxed) >= 2
        }));
        assert!(!handle.metrics().ready.load(Ordering::Acquire));
        assert!(first.frames.is_active());
        assert!(!second.frames.is_active());

        // 第二路的时间戳比第一路小，仍是该路合法首帧。
        second.push(Ok(Some(frame(700, 2))));
        assert!(crate::test_util::wait_until(Duration::from_secs(2), || {
            handle.metrics().ready.load(Ordering::Acquire)
        }));
        assert_eq!(handle.metrics().frames.load(Ordering::Relaxed), 2);
        assert_eq!(first.frames.try_take().unwrap().stamp_ns, 900);
        assert_eq!(second.frames.try_take().unwrap().stamp_ns, 700);
        second.push(Ok(Some(frame(800, 3))));
        assert!(crate::test_util::wait_until(Duration::from_secs(2), || {
            handle.metrics().frames.load(Ordering::Relaxed) == 3
        }));
        assert_eq!(second.frames.try_take().unwrap().data, [3; 6]);
        assert_eq!(handle.metrics().last_frame_ns.load(Ordering::Relaxed), 900);

        second.push(Ok(Some(frame(750, 4))));
        assert!(crate::test_util::wait_until(Duration::from_secs(2), || {
            handle.metrics().dropped.load(Ordering::Relaxed) == 1
                && !handle.metrics().ready.load(Ordering::Acquire)
        }));
        assert!(first.frames.is_active());
        assert!(!second.frames.is_active());
        assert_eq!(handle.metrics().frames.load(Ordering::Relaxed), 3);
        second.push(Ok(Some(frame(850, 5))));
        assert!(crate::test_util::wait_until(Duration::from_secs(2), || {
            handle.metrics().ready.load(Ordering::Acquire)
        }));
        assert_eq!(second.frames.try_take().unwrap().stamp_ns, 850);
        assert!(!handle.metrics().startup_failed.load(Ordering::Acquire));
        handle.request_stop();
        handle.join().unwrap();
        assert_eq!(first.stops.load(Ordering::Relaxed), 1);
        assert_eq!(second.stops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn camera_failure_before_its_first_frame_is_not_masked_by_another_camera() {
        for failed_first in [false, true] {
            let healthy = CameraFixture::default();
            let failed = CameraFixture::default();
            healthy.push(Ok(Some(frame(10, 1))));
            failed.push(Err(RobotError::HardwareFault("模拟首帧前采集失败")));
            let cameras = if failed_first {
                vec![failed.input(None), healthy.input(None)]
            } else {
                vec![healthy.input(None), failed.input(None)]
            };
            let handle = spawn_cameras(Arc::new(MockClock::new(MonoRawNs(10))), cameras);
            assert!(crate::test_util::wait_until(Duration::from_secs(2), || {
                handle.metrics().tick_count.load(Ordering::Relaxed) >= 2
            }));
            assert!(handle.metrics().startup_failed.load(Ordering::Acquire));
            assert!(!handle.metrics().ready.load(Ordering::Acquire));
            assert_eq!(handle.metrics().frames.load(Ordering::Relaxed), 1);
            assert_eq!(handle.metrics().camera_errors.load(Ordering::Relaxed), 1);
            assert!(healthy.frames.is_active());
            assert!(!failed.frames.is_active());
            assert_eq!(failed.stops.load(Ordering::Relaxed), 1);
            handle.request_stop();
            handle.join().unwrap();
            assert_eq!(healthy.stops.load(Ordering::Relaxed), 1);
            assert_eq!(failed.stops.load(Ordering::Relaxed), 1);
        }
    }

    #[test]
    fn stale_camera_stops_while_another_camera_keeps_delivering_frames() {
        let clock = Arc::new(MockClock::new(MonoRawNs(0)));
        let stale = CameraFixture::default();
        let healthy = CameraFixture::default();
        stale.push(Ok(Some(frame(0, 1))));
        healthy.push(Ok(Some(frame(0, 2))));
        let handle = spawn_cameras(clock.clone(), vec![stale.input(None), healthy.input(None)]);
        assert!(crate::test_util::wait_until(Duration::from_secs(2), || {
            handle.metrics().ready.load(Ordering::Acquire)
        }));
        clock.set(MonoRawNs(CAMERA_MAX_AGE_NS));
        healthy.push(Ok(Some(frame(CAMERA_MAX_AGE_NS, 3))));
        assert!(crate::test_util::wait_until(Duration::from_secs(2), || {
            handle.metrics().frames.load(Ordering::Relaxed) == 3
        }));
        assert!(handle.metrics().ready.load(Ordering::Acquire));
        assert_eq!(stale.stops.load(Ordering::Relaxed), 0);

        clock.set(MonoRawNs(CAMERA_MAX_AGE_NS + 1));
        assert!(crate::test_util::wait_until(Duration::from_secs(2), || {
            stale.stops.load(Ordering::Relaxed) == 1
        }));
        assert!(!handle.metrics().ready.load(Ordering::Acquire));
        assert!(!handle.metrics().startup_failed.load(Ordering::Acquire));
        assert!(!stale.frames.is_active());
        assert!(stale.frames.try_take().is_none());
        assert!(healthy.frames.is_active());
        assert_eq!(healthy.stops.load(Ordering::Relaxed), 0);
        healthy.push(Ok(Some(frame(CAMERA_MAX_AGE_NS + 1, 4))));
        assert!(crate::test_util::wait_until(Duration::from_secs(2), || {
            handle.metrics().frames.load(Ordering::Relaxed) == 4
        }));
        assert_eq!(healthy.frames.try_take().unwrap().data, [4; 6]);
        assert!(!handle.metrics().ready.load(Ordering::Acquire));
        assert_eq!(handle.metrics().camera_errors.load(Ordering::Relaxed), 1);
        handle.request_stop();
        handle.join().unwrap();
        assert_eq!(stale.stops.load(Ordering::Relaxed), 1);
        assert_eq!(healthy.stops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn all_cameras_are_stopped_and_the_first_cleanup_error_is_preserved() {
        for fault_during_poll in [false, true] {
            let first = CameraFixture::default();
            let second = CameraFixture::default();
            let healthy = CameraFixture::default();
            for camera in [&first, &second, &healthy] {
                camera.push(Ok(Some(frame(0, 1))));
            }
            let first_error = RobotError::HardwareFault("第一路模拟回收失败");
            let second_error = RobotError::HardwareFault("第二路模拟回收失败");
            let handle = spawn_cameras(
                Arc::new(MockClock::new(MonoRawNs(0))),
                vec![
                    first.input(Some(first_error)),
                    second.input(Some(second_error)),
                    healthy.input(None),
                ],
            );
            assert!(crate::test_util::wait_until(Duration::from_secs(2), || {
                handle.metrics().ready.load(Ordering::Acquire)
            }));
            let metrics = Arc::clone(handle.metrics());
            if fault_during_poll {
                first.push(Err(RobotError::HardwareFault("第一路模拟采集失败")));
                // 等第一路完成回收，再触发第二路，保证首个清理错误的次序。
                assert!(crate::test_util::wait_until(Duration::from_secs(2), || {
                    first.stops.load(Ordering::Relaxed) == 1
                }));
                second.push(Err(RobotError::HardwareFault("第二路模拟采集失败")));
                assert!(crate::test_util::wait_until(Duration::from_secs(2), || {
                    second.stops.load(Ordering::Relaxed) == 1
                }));
                assert!(healthy.frames.is_active());
            }
            handle.request_stop();
            let error = handle.join().unwrap_err();
            assert_eq!(error.downcast_ref::<RobotError>(), Some(&first_error));
            assert!(metrics.cleanup_failed.load(Ordering::Acquire));
            assert!(!metrics.ready.load(Ordering::Acquire));
            for camera in [&first, &second, &healthy] {
                assert_eq!(camera.stops.load(Ordering::Relaxed), 1);
                assert!(!camera.frames.is_active());
            }
        }
    }

    #[test]
    fn spawn_then_stop_cleanly() {
        let handle = spawn_io_thread(
            IoThreadConfig::default(),
            test_clock(),
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .unwrap();
        thread::sleep(Duration::from_millis(30));
        handle.request_stop();
        handle.join().unwrap();
    }

    #[test]
    fn tick_count_increments() {
        use crate::test_util::wait_until;
        // 加速 poll 让测试短时间就看到多次 tick。
        let cfg = IoThreadConfig {
            poll_interval_ns: 1_000_000, // 1 ms
        };
        let handle = spawn_io_thread(
            cfg,
            test_clock(),
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .unwrap();
        let metrics = Arc::clone(handle.metrics());
        // 确定性轮询：等 tick_count >= 5（每 ms 一次，100ms 内绰绰有余）
        assert!(
            wait_until(Duration::from_secs(2), || {
                metrics.tick_count.load(Ordering::Relaxed) >= 5
            }),
            "io_thread 2s 内未 tick ≥ 5 次，实际 {}",
            metrics.tick_count.load(Ordering::Relaxed)
        );
        assert!(metrics.ready.load(Ordering::Acquire));
        handle.request_stop();
        handle.join().unwrap();
    }

    #[test]
    fn stops_promptly() {
        use crate::test_util::wait_until;
        let handle = spawn_io_thread(
            IoThreadConfig::default(),
            test_clock(),
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .unwrap();
        let metrics = Arc::clone(handle.metrics());
        // 等线程跑起来（至少 1 tick）再 request_stop —— 比固定 5ms sleep 稳
        assert!(
            wait_until(Duration::from_secs(2), || {
                metrics.tick_count.load(Ordering::Relaxed) > 0
            }),
            "io_thread 2s 内未跑出第一 tick"
        );
        handle.request_stop();
        let start = std::time::Instant::now();
        handle.join().unwrap();
        // 退出延迟仍按 50ms 上限（poll_interval default 是 1ms 量级）
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "io_thread join too slow: {:?}",
            start.elapsed()
        );
    }

    /// **A2 freeze hook 单测**：设 `freeze_until_ns` 到未来 80ms，期间
    /// `tick_count` 不增长（spin 期间 worker 卡住）；过期后恢复增长。
    #[test]
    fn freeze_hook_holds_tick_count() {
        let cfg = IoThreadConfig {
            poll_interval_ns: 1_000_000, // 1 ms
        };
        let clock = test_clock();
        let handle = spawn_io_thread(
            cfg,
            Arc::clone(&clock),
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .unwrap();
        let metrics = Arc::clone(handle.metrics());

        // 等 worker 跑起来
        thread::sleep(Duration::from_millis(20));
        let pre_freeze = metrics.tick_count.load(Ordering::Relaxed);
        assert!(pre_freeze > 0, "worker 未启动，tick_count={pre_freeze}");

        // 设 freeze 80ms
        let until = clock.now().0 + 80_000_000;
        metrics.freeze_until_ns.store(until, Ordering::Release);

        // freeze 中段（40ms 后）确认 tick_count 没增多少
        thread::sleep(Duration::from_millis(40));
        let mid_freeze = metrics.tick_count.load(Ordering::Relaxed);
        let mid_grew = mid_freeze.saturating_sub(pre_freeze);
        assert!(
            mid_grew <= 1,
            "freeze 期间 tick_count 不应继续增长：pre={pre_freeze} mid={mid_freeze} 增 {mid_grew}",
        );

        // 等 freeze 过期 + 一些 tick 时间
        thread::sleep(Duration::from_millis(80));
        let post_freeze = metrics.tick_count.load(Ordering::Relaxed);
        assert!(
            post_freeze > mid_freeze + 5,
            "freeze 过期后 tick_count 应当恢复增长：mid={mid_freeze} post={post_freeze}",
        );

        handle.request_stop();
        handle.join().unwrap();
    }
}
