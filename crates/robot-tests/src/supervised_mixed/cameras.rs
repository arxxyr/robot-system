//! 同一发送端的相机订阅与20秒窗口证据；控制期间只保留内存原帧。

use super::{
    Iceoryx2Subscriber, LatestSubscriber, OpenOptions, PULSE_NS, RobotClock, SensorSection,
    Service, SystemClock, TopicClass, Value, Write, add, json, markers, msg, subscriber,
};
use std::path::Path;

const FRAME_FRESH_NS: u64 = 1_000_000_000;

#[derive(Clone, Copy)]
struct FrameObservation {
    stamp_ns: u64,
    observed_ns: u64,
}

struct CameraRecord {
    config: SensorSection,
    last: Option<FrameObservation>,
    window: Option<(u64, u64)>,
    frames: Vec<FrameObservation>,
    pixels: Vec<u8>,
    snapshot: Value,
}

impl CameraRecord {
    fn new(config: &SensorSection) -> Self {
        let frame_len = usize::try_from(u64::from(config.width) * 3 * u64::from(config.height))
            .expect("相机配置已校验帧长");
        Self {
            config: config.clone(),
            last: None,
            window: None,
            frames: Vec::with_capacity(1024),
            pixels: vec![0; frame_len],
            snapshot: Value::Null,
        }
    }

    fn step(&self) -> u32 {
        // 配置已经复用生产校验，宽度乘3和整帧长度均在16MiB上限内。
        self.config.width * 3
    }

    fn frame_len(&self) -> usize {
        usize::try_from(u64::from(self.step()) * u64::from(self.config.height))
            .expect("相机配置已校验帧长")
    }

    fn check_age(&self, now: u64) -> Result<(), String> {
        if self.last.is_some_and(|last| {
            now.checked_sub(last.stamp_ns)
                .is_none_or(|age| age > FRAME_FRESH_NS)
                || now < last.observed_ns
        }) {
            return Err(format!("{} 相机失流超过1秒或时钟回退", self.config.id));
        }
        if let Some((start, _)) = self.window
            && self.frames.is_empty()
            && now > add(start, FRAME_FRESH_NS)?
        {
            return Err(format!("{} 缺少活动首秒图像", self.config.id));
        }
        Ok(())
    }

    fn accept(
        &mut self,
        image: &msg::ImageHeader<'_>,
        header_stamp: u64,
        bytes: &[u8],
        observed: u64,
    ) -> Result<(), String> {
        if let Some((_, end)) = self.window
            && observed >= end
        {
            return self.check_age(end);
        }
        // 在接纳新帧前复查旧帧，不能用刚恢复的新帧掩盖已经超过一秒的失流。
        self.check_age(observed)?;
        let stamp = image.stamp_ns();
        if image.frame_id() != Some(self.config.id.as_str())
            || image.width() != self.config.width
            || image.height() != self.config.height
            || image.step() != self.step()
            || image.encoding() != Some("bgr8")
            || image.color_order() != Some("BGR")
            || bytes.len() != self.frame_len()
            || image
                .blob()
                .is_none_or(|blob| usize::try_from(blob.size()).ok() != Some(bytes.len()))
            || stamp == 0
            || stamp != header_stamp
            || self
                .last
                .is_some_and(|last| stamp <= last.stamp_ns || observed <= last.observed_ns)
            || observed
                .checked_sub(stamp)
                .is_none_or(|age| age > FRAME_FRESH_NS)
        {
            return Err(format!("{} 图像格式、原帧长度或时间戳异常", self.config.id));
        }
        let frame = FrameObservation {
            stamp_ns: stamp,
            observed_ns: observed,
        };
        self.last = Some(frame);
        if self
            .window
            .is_some_and(|(start, end)| (start..=end).contains(&stamp))
        {
            self.frames.push(frame);
            self.pixels.copy_from_slice(bytes);
        }
        Ok(())
    }

    fn begin_window(&mut self, start: u64, end: u64) -> Result<(), String> {
        if end.checked_sub(start) != Some(PULSE_NS) || self.window.is_some() {
            return Err("相机窗口必须为本轮唯一的完整20秒".into());
        }
        self.window = Some((start, end));
        Ok(())
    }

    fn finish_window(&self) -> Result<(), String> {
        let (start, end) = self.window.ok_or("相机缺少活动窗口")?;
        self.check_age(end)?;
        if self
            .frames
            .first()
            .is_none_or(|frame| frame.stamp_ns < start || frame.stamp_ns > start + FRAME_FRESH_NS)
            || self
                .frames
                .last()
                .is_none_or(|frame| frame.stamp_ns < end - FRAME_FRESH_NS)
        {
            return Err(format!("{} 图像没有覆盖完整20秒活动窗口", self.config.id));
        }
        Ok(())
    }

    fn save_snapshot(&mut self, output: &Path) -> Result<(), String> {
        let Some(frame) = self.frames.last() else {
            return Ok(());
        };
        let path = output
            .parent()
            .ok_or("结果文件缺少父目录")?
            .join(format!("camera-{}.bgr", self.config.id));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| format!("{} 原始BGR文件创建失败：{error}", self.config.id))?;
        file.write_all(&self.pixels)
            .and_then(|()| file.sync_all())
            .map_err(|error| format!("{} 原始BGR文件落盘失败：{error}", self.config.id))?;
        self.snapshot = json!({
            "path":path,"bytes":self.pixels.len(),"stamp_ns":frame.stamp_ns,
            "encoding":"bgr8","color_order":"BGR","saved_at_ns":SystemClock.now().0,
        });
        Ok(())
    }

    fn report(&self) -> Value {
        json!({
            "id":self.config.id,"device":self.config.device,
            "width":self.config.width,"height":self.config.height,"step":self.step(),
            "fps_requested":self.config.fps,"encoding":"bgr8","color_order":"BGR",
            "frames":self.frames.iter().map(|frame|json!({
                "stamp_ns":frame.stamp_ns,"observed_ns":frame.observed_ns,
            })).collect::<Vec<_>>(),
            "snapshot":self.snapshot,
        })
    }
}

struct CameraStream {
    images: Iceoryx2Subscriber<markers::ImageHeader>,
    record: CameraRecord,
}

pub(super) struct CameraObservations {
    cameras: Vec<CameraStream>,
    window: Option<(u64, u64)>,
    finished: bool,
    failure: Option<String>,
}

impl CameraObservations {
    pub(super) fn open(
        node: &iceoryx2::node::Node<Service>,
        robot: &str,
        configs: &[SensorSection],
    ) -> Result<Self, String> {
        let cameras = configs
            .iter()
            .map(|config| {
                Ok(CameraStream {
                    images: subscriber(
                        node,
                        &format!("/{robot}/sensors/{}/color", config.id),
                        TopicClass::Sensor,
                    )
                    .map_err(|error| error.to_string())?,
                    record: CameraRecord::new(config),
                })
            })
            .collect::<Result<_, String>>()?;
        Ok(Self {
            cameras,
            window: None,
            finished: false,
            failure: None,
        })
    }

    pub(super) fn poll(&mut self) -> Result<(), String> {
        if self.finished {
            return Ok(());
        }
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        let result = self.poll_inner();
        if let Err(error) = &result {
            self.failure = Some(error.clone());
        }
        result
    }

    fn poll_inner(&mut self) -> Result<(), String> {
        for camera in &mut self.cameras {
            if let Some((_, end)) = self.window
                && SystemClock.now().0 >= end
            {
                camera.record.check_age(end)?;
                continue;
            }
            if let Some(lease) = camera.images.latest().map_err(|error| error.to_string())? {
                let view = lease.view().map_err(|error| error.to_string())?;
                let image: msg::ImageHeader = view.body.get();
                let bytes = view.blob.ok_or("相机消息缺少原始像素")?.bytes();
                let observed = SystemClock.now().0;
                camera
                    .record
                    .accept(&image, view.header.stamp_ns(), bytes, observed)?;
            }
            // 没有新帧时仍沿生产的一秒帧龄门检查，不能只在收到新图像时检查。
            let now = self
                .window
                .map_or(SystemClock.now().0, |(_, end)| SystemClock.now().0.min(end));
            camera.record.check_age(now)?;
        }
        Ok(())
    }

    pub(super) fn ready(&self, now: u64) -> bool {
        self.failure.is_none()
            && self
                .cameras
                .iter()
                .all(|camera| camera.record.last.is_some() && camera.record.check_age(now).is_ok())
    }

    pub(super) fn require_ready(&self, now: u64) -> Result<(), String> {
        if self.ready(now) {
            Ok(())
        } else {
            Err("Enable前尚未取得全部相机的一秒内新鲜图像".into())
        }
    }

    pub(super) fn begin_window(&mut self, start: u64, end: u64) -> Result<(), String> {
        self.require_ready(start)?;
        for camera in &mut self.cameras {
            camera.record.begin_window(start, end)?;
        }
        self.window = Some((start, end));
        Ok(())
    }

    pub(super) fn finish_window(&mut self) -> Result<(), String> {
        // 此后停止路径不再依赖相机；窗口不完整也交原finalizer收紧与观察断扭。
        self.finished = true;
        for camera in &self.cameras {
            camera.record.finish_window()?;
        }
        Ok(())
    }

    pub(super) fn save_snapshots(&mut self, output: &Path) -> Result<(), String> {
        for camera in &mut self.cameras {
            camera.record.save_snapshot(output)?;
        }
        Ok(())
    }

    pub(super) fn report(&self) -> Value {
        json!({
            "schema":"S0_CAMERA_OBSERVATIONS_V1",
            "window_started_ns":self.window.map(|(start, _)|start),
            "window_finished_ns":self.window.map(|(_, end)|end),
            "cameras":self.cameras.iter().map(|camera|camera.record.report()).collect::<Vec<_>>(),
        })
    }
}

#[cfg(test)]
#[path = "camera_tests.rs"]
mod tests;
