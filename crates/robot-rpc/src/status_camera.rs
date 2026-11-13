//! 从既有 RGB 观测镜像生成有界 JPEG 缓存，不创建正式 topic 或访问相机。

use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use iceoryx2::prelude::NodeBuilder;
use iceoryx2::service::ipc::Service;
use jpeg_encoder::{ColorType, Encoder};
use robot_core::time::{RobotClock, SystemClock};
use robot_msg::{capability, markers, msg};
use robot_transport::MessageView;
use robot_transport::iceoryx2::{ObserverSubscriber, observer_subscriber};
use serde::{Deserialize, Serialize};
use tokio::sync::{Semaphore, watch};

const MAX_HTTP_REQUESTS: usize = 8;
const MAX_DRAIN: usize = 8;
const MAX_BLOB_BYTES: usize = 16 * 1024 * 1024;
const WORKER_POLL: Duration = Duration::from_millis(20);
const REOPEN_NS: u64 = 1_000_000_000;

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct Config {
    pub(super) enabled: bool,
    pub(super) interval_ms: u64,
    pub(super) max_width: u16,
    pub(super) max_height: u16,
    pub(super) jpeg_quality: u8,
    pub(super) stale_after_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_ms: 1000,
            max_width: 320,
            max_height: 240,
            jpeg_quality: 70,
            stale_after_ms: 3000,
        }
    }
}

impl Config {
    pub(super) fn validate(&self) -> Result<(), String> {
        if !(1000..=60_000).contains(&self.interval_ms)
            || !(1..=640).contains(&self.max_width)
            || !(1..=480).contains(&self.max_height)
            || !(1..=95).contains(&self.jpeg_quality)
            || !(self.interval_ms..=120_000).contains(&self.stale_after_ms)
        {
            return Err("相机预览间隔、分辨率、JPEG质量或陈旧时限无效".into());
        }
        Ok(())
    }
}

#[derive(Clone)]
pub(super) struct CameraSource {
    pub(super) id: String,
    pub(super) label: String,
    pub(super) topic: String,
    pub(super) frame_id: String,
}

#[derive(Clone)]
struct JpegFrame {
    bytes: Bytes,
    stamp_ns: u64,
    width: u16,
    height: u16,
}

#[derive(Default)]
struct Cache {
    frame: Option<JpegFrame>,
    error: Option<String>,
}

#[derive(Clone)]
pub(super) struct CameraState {
    config: Config,
    sources: Arc<Vec<CameraSource>>,
    caches: Arc<Mutex<Vec<Cache>>>,
    requests: Arc<Semaphore>,
}

impl CameraState {
    pub(super) fn new(config: Config, sources: Vec<CameraSource>) -> Self {
        Self {
            caches: Arc::new(Mutex::new(
                sources.iter().map(|_| Cache::default()).collect(),
            )),
            sources: Arc::new(sources),
            config,
            requests: Arc::new(Semaphore::new(MAX_HTTP_REQUESTS)),
        }
    }

    pub(super) fn start(
        &self,
        shutdown: watch::Receiver<bool>,
    ) -> std::io::Result<JoinHandle<Result<(), String>>> {
        let state = self.clone();
        std::thread::Builder::new()
            .name("robot-camera-web".into())
            .spawn(move || state.run(&shutdown))
    }

    fn update(&self, index: usize, result: Result<JpegFrame, String>) -> Result<(), String> {
        let mut caches = self.caches.lock().map_err(|_| "相机缓存锁已损坏")?;
        let cache = &mut caches[index];
        match result {
            Ok(frame) => {
                cache.frame = Some(frame);
                cache.error = None;
            }
            Err(error) => cache.error = Some(error),
        }
        Ok(())
    }

    fn frame_at(&self, index: usize, now_ns: u64) -> Result<(JpegFrame, u64), String> {
        if !self.config.enabled {
            return Err("相机预览未启用".into());
        }
        let caches = self.caches.lock().map_err(|_| "相机缓存锁已损坏")?;
        let cache = &caches[index];
        if let Some(error) = &cache.error {
            return Err(error.clone());
        }
        let frame = cache.frame.as_ref().ok_or("尚未收到相机画面")?;
        let age_ms = frame_age(frame.stamp_ns, now_ns, self.config.stale_after_ms)?;
        // Bytes仅复制引用，持锁期间不编码、不复制像素或等待网络。
        Ok((frame.clone(), age_ms))
    }

    fn run(&self, shutdown: &watch::Receiver<bool>) -> Result<(), String> {
        if !self.config.enabled || *shutdown.borrow() {
            return Ok(());
        }
        self.config.validate()?;
        let mut config = iceoryx2::config::Config::default();
        config.global.node.cleanup_dead_nodes_on_creation = false;
        config.global.node.cleanup_dead_nodes_on_destruction = false;
        let node = match NodeBuilder::new().config(&config).create::<Service>() {
            Ok(node) => node,
            Err(error) => {
                let error = format!("相机观测节点创建失败：{error:?}");
                tracing::error!(%error, "相机预览worker启动失败");
                for index in 0..self.sources.len() {
                    self.update(index, Err(error.clone()))?;
                }
                return Err(error);
            }
        };
        let mut slots: Vec<_> = self.sources.iter().map(|_| Slot::default()).collect();
        let clock = SystemClock;
        while !*shutdown.borrow() && shutdown.has_changed().is_ok() {
            let now_ns = clock.now().0;
            for (index, (source, slot)) in self.sources.iter().zip(&mut slots).enumerate() {
                if *shutdown.borrow() {
                    break;
                }
                if slot.subscriber.is_none() && now_ns >= slot.reopen_at_ns {
                    slot.reopen_at_ns = now_ns.saturating_add(REOPEN_NS);
                    match observer_subscriber::<markers::ImageHeader>(&node, &source.topic) {
                        Ok(subscriber) => slot.subscriber = Some(subscriber),
                        Err(_) => {
                            self.update(index, Err("相机观测源尚不可用".into()))?;
                        }
                    }
                }
                if now_ns < slot.next_frame_ns {
                    continue;
                }
                slot.next_frame_ns = now_ns.saturating_add(self.config.interval_ms * 1_000_000);
                let Some(subscriber) = slot.subscriber.as_mut() else {
                    continue;
                };
                let result = take_latest(subscriber, source, &self.config);
                match result {
                    Err(error) => {
                        slot.subscriber = None;
                        slot.reopen_at_ns = now_ns.saturating_add(REOPEN_NS);
                        self.update(index, Err(error))?;
                    }
                    Ok(None) => {}
                    Ok(Some(Err(error))) => self.update(index, Err(error))?,
                    Ok(Some(Ok(frame))) => {
                        match frame.stamp_ns.cmp(&slot.last_stamp_ns) {
                            std::cmp::Ordering::Less => {
                                self.update(index, Err("相机源时间戳回退".into()))?;
                            }
                            std::cmp::Ordering::Equal => {}
                            std::cmp::Ordering::Greater => {
                                slot.last_stamp_ns = frame.stamp_ns;
                                // take_latest已归还全部共享内存样本，编码仅访问自有像素。
                                self.update(index, encode(frame, self.config.jpeg_quality))?;
                            }
                        }
                    }
                }
            }
            std::thread::sleep(WORKER_POLL);
        }
        // 订阅先于节点析构，不留下借用或后台采集任务。
        drop(slots);
        drop(node);
        Ok(())
    }
}

#[derive(Default)]
struct Slot {
    subscriber: Option<ObserverSubscriber<markers::ImageHeader>>,
    reopen_at_ns: u64,
    next_frame_ns: u64,
    last_stamp_ns: u64,
}

struct RgbFrame {
    pixels: Vec<u8>,
    stamp_ns: u64,
    width: u16,
    height: u16,
}

fn take_latest(
    subscriber: &mut ObserverSubscriber<markers::ImageHeader>,
    source: &CameraSource,
    config: &Config,
) -> Result<Option<Result<RgbFrame, String>>, String> {
    let latest = drain_latest(|| {
        subscriber
            .try_recv()
            .map_err(|error| format!("相机观测接收失败：{error}"))
    })?;
    let Some(sample) = latest else {
        return Ok(None);
    };
    // 取完帧后读取接收时刻，drain期间的新帧不能被误判成未来来源。
    let now_ns = SystemClock.now().0;
    let result = sample
        .view()
        .map_err(|error| format!("相机帧校验失败：{error}"))
        .and_then(|view| copy_frame(&view, source, config, now_ns));
    drop(sample);
    Ok(Some(result))
}

fn drain_latest<T, E>(mut receive: impl FnMut() -> Result<Option<T>, E>) -> Result<Option<T>, E> {
    let mut latest = None;
    // 镜像队列持续到帧也只取固定次数，不能占住整个worker。
    for _ in 0..MAX_DRAIN {
        let Some(sample) = receive()? else {
            break;
        };
        latest = Some(sample);
    }
    Ok(latest)
}

fn copy_frame(
    view: &MessageView<'_, markers::ImageHeader>,
    source: &CameraSource,
    config: &Config,
    now_ns: u64,
) -> Result<RgbFrame, String> {
    let image = view.body.get();
    let stamp_ns = image.stamp_ns();
    frame_age(stamp_ns, now_ns, config.stale_after_ms)?;
    if view.header.stamp_ns() != stamp_ns || image.frame_id() != Some(source.frame_id.as_str()) {
        return Err("相机帧身份或来源时间戳不匹配".into());
    }
    if view.body.raw().len() > 8192
        || view.header.capability_flags()
            & (capability::CAP_GPU_MEMORY_HANDLE
                | capability::CAP_BLOB_COMPRESSED_LZ4
                | capability::CAP_BLOB_COMPRESSED_ZSTD)
            != 0
    {
        return Err("相机帧包含不支持的媒体能力或过长头部".into());
    }
    let bytes = view.blob.as_ref().ok_or("相机帧缺少像素")?.bytes();
    let desc = image.blob().ok_or("相机帧缺少像素描述")?;
    if bytes.len() > MAX_BLOB_BYTES
        || desc.size() != bytes.len() as u64
        || desc.offset() != 0
        || desc.compression() != msg::Compression::None_
        || desc.dtype() != msg::Dtype::U8
        || desc.layout() != msg::BlobLayout::HWC
        || !matches!(desc.endian(), msg::Endian::Little | msg::Endian::Big)
    {
        return Err("相机像素描述无效或超出16MiB".into());
    }
    let width = image.width() as usize;
    let height = image.height() as usize;
    let step = image.step() as usize;
    if width == 0
        || height == 0
        || width.checked_mul(3).is_none_or(|row| step < row)
        || step.checked_mul(height) != Some(bytes.len())
        || desc.shape().is_none_or(|shape| {
            shape.len() != 3
                || shape.get(0) != image.height()
                || shape.get(1) != image.width()
                || shape.get(2) != 3
        })
    {
        return Err("相机分辨率、shape、step或像素长度不一致".into());
    }
    if let Some(strides) = desc.strides()
        && !strides.is_empty()
        && (strides.len() != 3
            || i64::from(strides.get(0)) != i64::from(image.step())
            || strides.get(1) != 3
            || strides.get(2) != 1)
    {
        return Err("相机像素strides与step不一致".into());
    }
    let bgr = match (image.encoding(), image.color_order().unwrap_or("")) {
        (Some("bgr8"), "" | "BGR") => true,
        (Some("rgb8"), "" | "RGB") => false,
        _ => return Err("相机预览只支持一致的RGB8/BGR8编码".into()),
    };
    let mut output_width = width.min(usize::from(config.max_width));
    let mut output_height = (height * output_width / width).max(1);
    if output_height > usize::from(config.max_height) {
        output_height = usize::from(config.max_height);
        output_width = (width * output_height / height).max(1);
    }
    let mut pixels = Vec::with_capacity(output_width * output_height * 3);
    for y in 0..output_height {
        let row = y * height / output_height * step;
        for x in 0..output_width {
            let at = row + x * width / output_width * 3;
            let [a, b, c] = [bytes[at], bytes[at + 1], bytes[at + 2]];
            let pixel = if bgr { [c, b, a] } else { [a, b, c] };
            pixels.extend_from_slice(&pixel);
        }
    }
    Ok(RgbFrame {
        pixels,
        stamp_ns,
        width: u16::try_from(output_width).map_err(|_| "预览宽度溢出")?,
        height: u16::try_from(output_height).map_err(|_| "预览高度溢出")?,
    })
}

fn frame_age(stamp_ns: u64, now_ns: u64, stale_after_ms: u64) -> Result<u64, String> {
    if stamp_ns == 0 || stamp_ns > now_ns {
        return Err("相机源时间戳无效或晚于本机时钟".into());
    }
    let age_ns = now_ns - stamp_ns;
    if age_ns > stale_after_ms * 1_000_000 {
        return Err("相机画面已陈旧".into());
    }
    Ok(age_ns / 1_000_000)
}

fn encode(frame: RgbFrame, quality: u8) -> Result<JpegFrame, String> {
    let RgbFrame {
        pixels,
        stamp_ns,
        width,
        height,
    } = frame;
    let mut bytes = Vec::new();
    Encoder::new(&mut bytes, quality)
        .encode(&pixels, width, height, ColorType::Rgb)
        .map_err(|error| format!("相机JPEG编码失败：{error}"))?;
    Ok(JpegFrame {
        bytes: Bytes::from(bytes),
        stamp_ns,
        width,
        height,
    })
}

#[derive(Serialize)]
struct CamerasView<'a> {
    enabled: bool,
    interval_ms: u64,
    stale_after_ms: u64,
    cameras: Vec<CameraView<'a>>,
}

#[derive(Serialize)]
struct CameraView<'a> {
    id: &'a str,
    label: &'a str,
}

#[derive(Serialize)]
struct CameraError {
    error: String,
}

fn error_response(status: StatusCode, error: impl Into<String>) -> Response {
    (
        status,
        Json(CameraError {
            error: error.into(),
        }),
    )
        .into_response()
}

async fn cameras(State(state): State<CameraState>) -> Response {
    Json(CamerasView {
        enabled: state.config.enabled,
        interval_ms: state.config.interval_ms,
        stale_after_ms: state.config.stale_after_ms,
        cameras: state
            .sources
            .iter()
            .map(|source| CameraView {
                id: &source.id,
                label: &source.label,
            })
            .collect(),
    })
    .into_response()
}

async fn frame(State(state): State<CameraState>, Path(id): Path<String>) -> Response {
    let Some(index) = state.sources.iter().position(|source| source.id == id) else {
        return error_response(StatusCode::NOT_FOUND, "未知相机");
    };
    let Ok(permit) = Arc::clone(&state.requests).try_acquire_owned() else {
        return error_response(StatusCode::TOO_MANY_REQUESTS, "相机请求已达到并发上限");
    };
    let (frame, age_ms) = match state.frame_at(index, SystemClock.now().0) {
        Ok(value) => value,
        Err(error) => return error_response(StatusCode::SERVICE_UNAVAILABLE, error),
    };
    let length = frame.bytes.len();
    // 许可随响应体释放；发送只引用缓存，不为每个客户端保留独立编码任务。
    let stream =
        futures_util::stream::unfold((Some(frame.bytes), permit), |(bytes, permit)| async {
            bytes.map(|bytes| (Ok::<_, Infallible>(bytes), (None, permit)))
        });
    let mut response = Body::from_stream(stream).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/jpeg"));
    for (name, value) in [
        ("content-length", length.to_string()),
        ("x-frame-stamp-ns", frame.stamp_ns.to_string()),
        ("x-frame-age-ms", age_ms.to_string()),
        ("x-frame-width", frame.width.to_string()),
        ("x-frame-height", frame.height.to_string()),
    ] {
        let Ok(value) = HeaderValue::from_str(&value) else {
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "相机响应头无效");
        };
        headers.insert(name, value);
    }
    response
}

pub(super) fn router(state: CameraState) -> Router {
    Router::new()
        .route("/api/cameras", axum::routing::get(cameras))
        .route("/api/cameras/{id}/frame", axum::routing::get(frame))
        .layer(axum::middleware::from_fn(
            |request: axum::extract::Request, next: axum::middleware::Next| async {
                let mut response = next.run(request).await;
                response
                    .headers_mut()
                    .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
                response.headers_mut().insert(
                    header::X_CONTENT_TYPE_OPTIONS,
                    HeaderValue::from_static("nosniff"),
                );
                response
            },
        ))
        .with_state(state)
}

#[cfg(test)]
#[path = "status_camera_tests.rs"]
mod tests;
