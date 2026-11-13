//! 用软件消息、隔离镜像和HTTP路由验证预览，不启动相机或硬件服务。

use super::*;
use axum::body::to_bytes;
use axum::http::Request;
use robot_transport::mock::MockChannel;
use robot_transport::{LatestSubscriber, Publisher};
use std::sync::atomic::{AtomicUsize, Ordering};
use tower::ServiceExt;

fn source() -> CameraSource {
    CameraSource {
        id: "bus1:head_color".into(),
        label: "头部".into(),
        topic: "/robot/sensors/head_color/color".into(),
        frame_id: "head_color".into(),
    }
}

fn config() -> Config {
    Config {
        enabled: true,
        ..Config::default()
    }
}

struct Fixture<'a> {
    frame_id: &'a str,
    encoding: &'a str,
    color_order: &'a str,
    stamp_ns: u64,
    width: u32,
    height: u32,
    step: u32,
    shape: Vec<u32>,
    strides: Vec<i32>,
    compression: msg::Compression,
    dtype: msg::Dtype,
}

impl Default for Fixture<'_> {
    fn default() -> Self {
        Self {
            frame_id: "head_color",
            encoding: "bgr8",
            color_order: "BGR",
            stamp_ns: 1000,
            width: 4,
            height: 2,
            step: 14,
            shape: vec![2, 4, 3],
            strides: vec![14, 3, 1],
            compression: msg::Compression::None_,
            dtype: msg::Dtype::U8,
        }
    }
}

fn image_body(fixture: &Fixture<'_>, bytes_len: usize) -> Vec<u8> {
    let mut builder = flatbuffers::FlatBufferBuilder::new();
    let shape = builder.create_vector(&fixture.shape);
    let strides = builder.create_vector(&fixture.strides);
    let desc = msg::BlobDesc::create(
        &mut builder,
        &msg::BlobDescArgs {
            size: bytes_len as u64,
            alignment: 1,
            shape: Some(shape),
            strides: Some(strides),
            compression: fixture.compression,
            dtype: fixture.dtype,
            ..Default::default()
        },
    );
    let frame_id = builder.create_string(fixture.frame_id);
    let encoding = builder.create_string(fixture.encoding);
    let order = builder.create_string(fixture.color_order);
    let image = msg::ImageHeader::create(
        &mut builder,
        &msg::ImageHeaderArgs {
            blob: Some(desc),
            width: fixture.width,
            height: fixture.height,
            step: fixture.step,
            frame_id: Some(frame_id),
            encoding: Some(encoding),
            color_order: Some(order),
            stamp_ns: fixture.stamp_ns,
        },
    );
    builder.finish(image, None);
    builder.finished_data().to_vec()
}

fn attempt(
    fixture: &Fixture<'_>,
    bytes: &[u8],
    stamp_ns: u64,
    now_ns: u64,
) -> Result<RgbFrame, String> {
    let channel = MockChannel::<markers::ImageHeader>::new();
    let mut publisher = channel.publisher();
    let mut subscriber = channel.subscriber();
    let mut writer = publisher.loan(8192).unwrap();
    writer.set_stamp_ns(stamp_ns);
    writer.set_body_bytes(&image_body(fixture, bytes.len()));
    writer.set_blob_bytes(bytes);
    writer.publish().unwrap();
    let lease = subscriber.latest().unwrap().unwrap();
    let result = lease
        .view()
        .map_err(|error| error.to_string())
        .and_then(|view| {
            copy_frame(
                &view,
                &source(),
                &Config {
                    max_width: 2,
                    max_height: 1,
                    ..config()
                },
                now_ns,
            )
        });
    drop(lease);
    assert_eq!(channel.active_leases(), 0);
    result
}

#[test]
fn preview_config_has_bounded_explicit_defaults() {
    let defaults = Config::default();
    assert!(!defaults.enabled);
    assert_eq!(defaults.interval_ms, 1000);
    assert_eq!((defaults.max_width, defaults.max_height), (320, 240));
    assert_eq!(defaults.jpeg_quality, 70);
    assert_eq!(defaults.stale_after_ms, 3000);
    assert!(defaults.validate().is_ok());
    for invalid in [
        Config {
            interval_ms: 999,
            ..config()
        },
        Config {
            interval_ms: 60_001,
            ..config()
        },
        Config {
            max_width: 0,
            ..config()
        },
        Config {
            max_width: 641,
            ..config()
        },
        Config {
            max_height: 0,
            ..config()
        },
        Config {
            max_height: 481,
            ..config()
        },
        Config {
            jpeg_quality: 0,
            ..config()
        },
        Config {
            jpeg_quality: 96,
            ..config()
        },
        Config {
            stale_after_ms: 999,
            ..config()
        },
        Config {
            stale_after_ms: 120_001,
            ..config()
        },
    ] {
        assert!(invalid.validate().is_err());
    }
    assert!(toml::from_str::<Config>("enabled = true\nunknown = 1").is_err());
    assert!(toml::from_str::<Config>("enabled = true").unwrap().enabled);
}

#[test]
fn bgr_downsampling_preserves_source_stamp_pixels_and_releases_loan() {
    let bytes = [
        1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 99, 99, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31,
        32, 99, 99,
    ];
    let frame = attempt(&Fixture::default(), &bytes, 1000, 2000).unwrap();
    assert_eq!((frame.width, frame.height, frame.stamp_ns), (2, 1, 1000));
    assert_eq!(frame.pixels, [3, 2, 1, 9, 8, 7]);
    let jpeg = encode(frame, 70).unwrap();
    assert_eq!(&jpeg.bytes[..2], &[0xff, 0xd8]);
    assert_eq!(&jpeg.bytes[jpeg.bytes.len() - 2..], &[0xff, 0xd9]);
    let fixture = Fixture {
        encoding: "rgb8",
        color_order: "RGB",
        ..Fixture::default()
    };
    assert_eq!(
        attempt(&fixture, &bytes, 1000, 2000).unwrap().pixels,
        [1, 2, 3, 7, 8, 9]
    );
}

#[test]
fn image_identity_clock_encoding_shape_and_stride_are_validated() {
    let bytes = [1; 28];
    for fixture in [
        Fixture {
            frame_id: "other_camera",
            ..Fixture::default()
        },
        Fixture {
            stamp_ns: 0,
            ..Fixture::default()
        },
        Fixture {
            stamp_ns: 2001,
            ..Fixture::default()
        },
        Fixture {
            encoding: "yuyv",
            ..Fixture::default()
        },
        Fixture {
            color_order: "RGB",
            ..Fixture::default()
        },
        Fixture {
            width: 0,
            ..Fixture::default()
        },
        Fixture {
            height: 0,
            ..Fixture::default()
        },
        Fixture {
            step: 11,
            ..Fixture::default()
        },
        Fixture {
            shape: vec![2, 3, 3],
            ..Fixture::default()
        },
        Fixture {
            strides: vec![14, 1, 3],
            ..Fixture::default()
        },
        Fixture {
            compression: msg::Compression::Zstd,
            ..Fixture::default()
        },
        Fixture {
            dtype: msg::Dtype::U16,
            ..Fixture::default()
        },
    ] {
        assert!(attempt(&fixture, &bytes, fixture.stamp_ns, 2000).is_err());
    }
    assert!(attempt(&Fixture::default(), &bytes, 999, 2000).is_err());
    assert!(attempt(&Fixture::default(), &bytes[..27], 1000, 2000).is_err());
    assert!(attempt(&Fixture::default(), &bytes, 1000, 3_000_001_001).is_err());
}

#[test]
fn drain_keeps_only_latest_and_bounds_continuous_input() {
    let count = AtomicUsize::new(0);
    let result =
        drain_latest::<_, Infallible>(|| Ok(Some(count.fetch_add(1, Ordering::Relaxed)))).unwrap();
    assert_eq!(count.load(Ordering::Relaxed), MAX_DRAIN);
    assert_eq!(result, Some(MAX_DRAIN - 1));
    let mut values = [1, 2, 3].into_iter();
    assert_eq!(
        drain_latest::<_, Infallible>(|| Ok(values.next())).unwrap(),
        Some(3)
    );
    assert_eq!(drain_latest::<u8, _>(|| Err("接收失败")), Err("接收失败"));
}

fn seed(state: &CameraState, stamp_ns: u64) {
    state
        .update(
            0,
            Ok(JpegFrame {
                bytes: Bytes::from_static(&[0xff, 0xd8, 0xff, 0xd9]),
                stamp_ns,
                width: 320,
                height: 240,
            }),
        )
        .unwrap();
}

#[test]
fn cache_age_uses_source_clock_and_errors_hide_previous_frame() {
    let state = CameraState::new(config(), vec![source()]);
    assert!(state.frame_at(0, 1000).is_err());
    seed(&state, 1000);
    assert_eq!(state.frame_at(0, 2_000_001_000).unwrap().1, 2000);
    assert!(state.frame_at(0, 999).is_err());
    assert!(state.frame_at(0, 3_000_001_001).is_err());
    state.update(0, Err("来源错误".into())).unwrap();
    assert_eq!(state.frame_at(0, 2000).err().as_deref(), Some("来源错误"));
    seed(&state, 2000);
    assert!(state.frame_at(0, 2001).is_ok());
}

fn request(path: &str) -> Request<Body> {
    Request::builder().uri(path).body(Body::empty()).unwrap()
}

#[tokio::test]
async fn routes_report_metadata_jpeg_headers_disabled_unknown_and_stale() {
    let state = CameraState::new(config(), vec![source()]);
    let (mut web, _latest, _shutdown, _incoming) = super::super::tests::web_state();
    web.cameras = state.clone();
    let app = super::super::router(web);
    let response = app.clone().oneshot(request("/api/cameras")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 8192).await.unwrap()).unwrap();
    assert_eq!(json["enabled"], true);
    assert_eq!(json["interval_ms"], 1000);
    assert_eq!(json["cameras"][0]["id"], "bus1:head_color");
    assert_eq!(json["cameras"][0]["label"], "头部");
    let path = "/api/cameras/bus1:head_color/frame";
    assert_eq!(
        app.clone().oneshot(request(path)).await.unwrap().status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    let stamp = SystemClock.now().0;
    seed(&state, stamp);
    let response = app.clone().oneshot(request(path)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "image/jpeg");
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(response.headers()["x-frame-stamp-ns"], stamp.to_string());
    assert_eq!(response.headers()["x-frame-width"], "320");
    assert_eq!(response.headers()["x-frame-height"], "240");
    assert_eq!(
        to_bytes(response.into_body(), 8192).await.unwrap(),
        [0xff, 0xd8, 0xff, 0xd9].as_slice()
    );
    assert_eq!(
        app.clone()
            .oneshot(request("/api/cameras/missing/frame"))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    seed(&state, 1);
    assert_eq!(
        app.oneshot(request(path)).await.unwrap().status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    let state = CameraState::new(Config::default(), vec![source()]);
    seed(&state, SystemClock.now().0);
    assert_eq!(
        router(state).oneshot(request(path)).await.unwrap().status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[tokio::test]
async fn image_permits_last_until_response_bodies_are_released() {
    let state = CameraState::new(config(), vec![source()]);
    seed(&state, SystemClock.now().0);
    let (mut web, _latest, _shutdown, _incoming) = super::super::tests::web_state();
    web.cameras = state.clone();
    let app = super::super::router(web);
    let path = "/api/cameras/bus1:head_color/frame";
    let mut responses = Vec::new();
    for _ in 0..MAX_HTTP_REQUESTS {
        let response = app.clone().oneshot(request(path)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        responses.push(response);
    }
    assert_eq!(state.requests.available_permits(), 0);
    assert_eq!(
        app.clone().oneshot(request(path)).await.unwrap().status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    // 元数据请求独立于图像下载许可。
    assert_eq!(
        app.clone()
            .oneshot(request("/api/cameras"))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        app.clone()
            .oneshot(request("/api/state"))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    responses.pop();
    assert_eq!(state.requests.available_permits(), 1);
    let response = app.clone().oneshot(request(path)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    drop(response);
    drop(responses);
    assert_eq!(state.requests.available_permits(), MAX_HTTP_REQUESTS);
}

#[test]
fn disabled_worker_exits_without_opening_a_node() {
    let state = CameraState::new(Config::default(), vec![source()]);
    let (_sender, receiver) = watch::channel(false);
    state.start(receiver).unwrap().join().unwrap().unwrap();
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn existing_transport_mirror_reaches_worker_jpeg_and_http_and_closes() {
    use robot_transport::iceoryx2::{TopicClass, publisher};

    static NEXT_TOPIC: AtomicUsize = AtomicUsize::new(0);
    let mut source = source();
    source.topic = format!(
        "/preview_test_{}_{}/sensors/head_color/color",
        std::process::id(),
        NEXT_TOPIC.fetch_add(1, Ordering::Relaxed)
    );
    let mut node_config = iceoryx2::config::Config::default();
    node_config.global.node.cleanup_dead_nodes_on_creation = false;
    node_config.global.node.cleanup_dead_nodes_on_destruction = false;
    let node = NodeBuilder::new()
        .config(&node_config)
        .create::<Service>()
        .unwrap();
    let mut publisher =
        publisher::<markers::ImageHeader>(&node, &source.topic, TopicClass::Sensor).unwrap();
    let monitor = publisher.observer_monitor();
    let state = CameraState::new(config(), vec![source]);
    let (stop, stopped) = watch::channel(false);
    let worker = state.start(stopped).unwrap();
    let clock = SystemClock;
    let deadline = clock.now().0 + 5_000_000_000;
    let bytes = [80; 28];
    let mut received = None;
    let mut publish_error = None;
    while clock.now().0 < deadline {
        let stamp = clock.now().0;
        let body = image_body(
            &Fixture {
                stamp_ns: stamp,
                ..Fixture::default()
            },
            bytes.len(),
        );
        let result = publisher.loan(8192).and_then(|mut writer| {
            writer.set_stamp_ns(stamp);
            writer.set_body_bytes(&body);
            writer.set_blob_bytes(&bytes);
            writer.publish()
        });
        if let Err(error) = result {
            publish_error = Some(error.to_string());
            break;
        }
        if let Ok((frame, _)) = state.frame_at(0, clock.now().0) {
            received = Some(frame);
            break;
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    let _ = stop.send(true);
    // 软件测试等待真实join，不能以句柄drop代替worker已退出。
    worker.join().unwrap().unwrap();
    let observation = monitor.snapshot();
    drop(publisher);
    assert!(monitor.snapshot().reaped);
    drop(node);
    assert!(publish_error.is_none(), "假发布失败：{publish_error:?}");
    let received = received.expect("5秒内没有从已有镜像取得JPEG");
    assert!(observation.published > 0);
    assert_eq!((received.width, received.height), (4, 2));
    assert!(received.stamp_ns > 0);
    let response = router(state)
        .oneshot(request("/api/cameras/bus1:head_color/frame"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["x-frame-stamp-ns"],
        received.stamp_ns.to_string()
    );
    assert_eq!(
        to_bytes(response.into_body(), 8192).await.unwrap(),
        received.bytes
    );
}
