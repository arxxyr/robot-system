use super::*;
use std::path::PathBuf;

fn config() -> SensorSection {
    SensorSection {
        hardware_id: "fixture_hardware".into(),
        id: "fixture_camera".into(),
        adapter: "opencv_v4l2".into(),
        device: PathBuf::from("/software-fixture/video"),
        python: PathBuf::from("/software-fixture/python"),
        helper: PathBuf::from("/software-fixture/helper.py"),
        width: 2,
        height: 1,
        fps: 10,
    }
}

struct ImageFixture<'a> {
    stamp: u64,
    width: u32,
    height: u32,
    step: u32,
    frame_id: &'a str,
    encoding: &'a str,
    color_order: &'a str,
    blob_len: u64,
}

impl ImageFixture<'_> {
    fn new(stamp: u64) -> Self {
        Self {
            stamp,
            width: 2,
            height: 1,
            step: 6,
            frame_id: "fixture_camera",
            encoding: "bgr8",
            color_order: "BGR",
            blob_len: 6,
        }
    }

    fn encode(&self) -> Vec<u8> {
        let mut builder = flatbuffers::FlatBufferBuilder::new();
        let blob = msg::BlobDesc::create(
            &mut builder,
            &msg::BlobDescArgs {
                size: self.blob_len,
                ..Default::default()
            },
        );
        let frame_id = builder.create_string(self.frame_id);
        let encoding = builder.create_string(self.encoding);
        let color_order = builder.create_string(self.color_order);
        let image = msg::ImageHeader::create(
            &mut builder,
            &msg::ImageHeaderArgs {
                blob: Some(blob),
                width: self.width,
                height: self.height,
                step: self.step,
                frame_id: Some(frame_id),
                encoding: Some(encoding),
                color_order: Some(color_order),
                stamp_ns: self.stamp,
            },
        );
        builder.finish(image, None);
        builder.finished_data().to_vec()
    }
}

fn accept(record: &mut CameraRecord, stamp: u64, observed: u64) -> Result<(), String> {
    let bytes = ImageFixture::new(stamp).encode();
    let image = flatbuffers::root::<msg::ImageHeader>(&bytes).unwrap();
    record.accept(&image, stamp, &[1, 2, 3, 4, 5, 6], observed)
}

#[test]
fn camera_frame_checks_configuration_encoding_blob_and_both_stamps() {
    for changed in [
        "id", "width", "height", "step", "encoding", "order", "blob", "zero",
    ] {
        let mut fixture = ImageFixture::new(100);
        match changed {
            "id" => fixture.frame_id = "different_camera",
            "width" => fixture.width = 3,
            "height" => fixture.height = 2,
            "step" => fixture.step = 8,
            "encoding" => fixture.encoding = "rgb8",
            "order" => fixture.color_order = "RGB",
            "blob" => fixture.blob_len = 5,
            "zero" => fixture.stamp = 0,
            _ => unreachable!(),
        }
        let bytes = fixture.encode();
        let image = flatbuffers::root::<msg::ImageHeader>(&bytes).unwrap();
        assert!(
            CameraRecord::new(&config())
                .accept(&image, fixture.stamp, &[0; 6], 101)
                .is_err(),
            "必须拒绝{changed}"
        );
    }
    let bytes = ImageFixture::new(100).encode();
    let image = flatbuffers::root::<msg::ImageHeader>(&bytes).unwrap();
    assert!(
        CameraRecord::new(&config())
            .accept(&image, 99, &[0; 6], 101)
            .is_err()
    );
    assert!(
        CameraRecord::new(&config())
            .accept(&image, 100, &[0; 5], 101)
            .is_err()
    );
    assert!(
        CameraRecord::new(&config())
            .accept(&image, 100, &[0; 6], 99)
            .is_err()
    );
    assert!(
        CameraRecord::new(&config())
            .accept(&image, 100, &[0; 6], 100 + FRAME_FRESH_NS + 1)
            .is_err()
    );
}

#[test]
fn camera_time_must_advance_and_new_frames_cannot_hide_prior_loss() {
    let mut record = CameraRecord::new(&config());
    accept(&mut record, 100, 101).unwrap();
    assert!(accept(&mut record, 100, 102).is_err());
    assert!(accept(&mut record, 99, 103).is_err());
    assert!(accept(&mut record, 101, 101).is_err());
    assert!(record.check_age(100 + FRAME_FRESH_NS).is_ok());
    assert!(record.check_age(101 + FRAME_FRESH_NS).is_err());
    assert!(record.check_age(100).is_err());
    assert!(accept(&mut record, 101 + FRAME_FRESH_NS, 102 + FRAME_FRESH_NS).is_err());
}

#[test]
fn camera_window_uses_active_frames_and_one_second_bounds_without_forced_fps() {
    let start = 2 * FRAME_FRESH_NS;
    let end = start + PULSE_NS;
    let mut record = CameraRecord::new(&config());
    accept(&mut record, start - 1, start - 1).unwrap();
    record.begin_window(start, end).unwrap();
    for index in 0..40 {
        let stamp = start + index * FRAME_FRESH_NS / 2;
        accept(&mut record, stamp, stamp + 1).unwrap();
    }
    record.finish_window().unwrap();
    assert_eq!(record.frames.len(), 40);
    assert_eq!(record.frames[0].stamp_ns, start);
    assert!(record.frames.last().unwrap().stamp_ns >= end - FRAME_FRESH_NS);
    let original = record.pixels.clone();
    accept(&mut record, end + FRAME_FRESH_NS, end + FRAME_FRESH_NS + 1).unwrap();
    assert_eq!(record.frames.len(), 40);
    assert_eq!(record.pixels, original);
    let mut missing = CameraRecord::new(&config());
    accept(&mut missing, start - 1, start - 1).unwrap();
    missing.begin_window(start, end).unwrap();
    assert!(missing.check_age(start + FRAME_FRESH_NS + 1).is_err());
    assert!(missing.finish_window().is_err());
    assert!(missing.begin_window(start, end).is_err());
    assert!(
        CameraRecord::new(&config())
            .begin_window(start, end - 1)
            .is_err()
    );
}

#[test]
fn camera_snapshot_is_the_last_active_raw_frame_and_never_overwrites() {
    let directory = tempfile::tempdir().unwrap();
    let output = directory.path().join("sender.json");
    let mut record = CameraRecord::new(&config());
    record.begin_window(100, 100 + PULSE_NS).unwrap();
    accept(&mut record, 101, 102).unwrap();
    let bytes = ImageFixture::new(102).encode();
    let image = flatbuffers::root::<msg::ImageHeader>(&bytes).unwrap();
    record
        .accept(&image, 102, &[6, 5, 4, 3, 2, 1], 103)
        .unwrap();
    record.save_snapshot(&output).unwrap();
    let report = record.report();
    let path = PathBuf::from(report["snapshot"]["path"].as_str().unwrap());
    assert_eq!(path, directory.path().join("camera-fixture_camera.bgr"));
    assert_eq!(std::fs::read(&path).unwrap(), [6, 5, 4, 3, 2, 1]);
    assert_eq!(report["snapshot"]["bytes"], 6);
    assert_eq!(report["snapshot"]["stamp_ns"], 102);
    assert_eq!(report["snapshot"]["encoding"], "bgr8");
    assert!(record.save_snapshot(&output).is_err());
    assert_eq!(std::fs::read(path).unwrap(), [6, 5, 4, 3, 2, 1]);
}
