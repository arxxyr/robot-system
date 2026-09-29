//! 真实 SDK 输出。每帧同步确认 flush 后才接收下一帧，连接失败不离线积压。

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use rerun::blueprint::{
    Blueprint, BlueprintOpts, ContainerLike, Grid, Spatial2DView, Spatial3DView, Tabs, TensorView,
    TextLogView, TimePanel, TimeSeriesView,
};
use rerun::encodings::{TensorBuffer, TensorData};
use rerun::external::re_log_types::EntityPathPart;

use crate::decode::{Media, Observation};
use crate::media::Pixels;

const FLUSH_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_VIEW_TOPICS: usize = 64;
const MAX_VIEW_INSTANCES: usize = 8;

#[derive(Default)]
struct SourceViews {
    schema: u32,
    instances: BTreeMap<String, rerun::EntityPath>,
}

type Views = BTreeMap<String, SourceViews>;

/// 独立录制通道；不共享任何控制进程对象。
pub struct Output {
    recording: rerun::RecordingStream,
    views: Views,
    blueprint_dirty: bool,
}

impl Output {
    pub fn save(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        Self::from_recording(builder().save(path)?)
    }

    pub fn connect(url: &str) -> Result<Self, Box<dyn std::error::Error>> {
        Self::from_recording(builder().connect_grpc_opts(url)?)
    }

    /// 也用于官方 memory sink 的纯软件内容验证。
    pub fn from_recording(
        recording: rerun::RecordingStream,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        recording.set_log_time_enabled(false);
        recording.set_log_tick_enabled(false);
        recording.log_static("bridge/说明", &rerun::TextDocument::new("OBSERVATION_ONLY. Lossy mirror, never a physical safety confirmation. RobotClock time. NO_DATA / STALE / UNKNOWN_NOT_SAFE. velocity_wire and effort_wire retain driver values; depth16 has no assumed physical unit."))?;
        // 首次 flush 同时验证实际输出；无 viewer 时明确失败，不能静默缓冲。
        recording.flush_with_timeout(FLUSH_TIMEOUT)?;
        Ok(Self {
            recording,
            views: BTreeMap::new(),
            blueprint_dirty: false,
        })
    }

    pub fn log(&mut self, observation: Observation) -> Result<(), Box<dyn std::error::Error>> {
        let root = root(&observation.topic)
            / "instances"
            / EntityPathPart::from(observation.publisher_instance.clone());
        if observation.media.is_some() || !observation.series.is_empty() {
            self.register_view(&observation, root.clone())?;
        }
        self.flush_blueprint()?;
        self.recording
            .set_time_sequence("source_seq", observation.seq);
        self.recording.set_time(
            "robot_time",
            rerun::external::re_log_types::TimeCell::from_duration_nanos(observation.body_stamp_ns),
        );
        self.recording.set_time(
            "published_time",
            rerun::external::re_log_types::TimeCell::from_duration_nanos(
                observation.header_stamp_ns,
            ),
        );
        self.recording.set_time(
            "received_time",
            rerun::external::re_log_types::TimeCell::from_duration_nanos(observation.received_ns),
        );
        for series in observation.series {
            let path = series.path.iter().fold(root.clone(), |parent, child| {
                parent / EntityPathPart::from(child.clone())
            });
            self.recording
                .log(path, &rerun::Scalars::new([series.value]))?;
        }
        self.recording.log(
            root.clone() / "status",
            &rerun::TextLog::new(observation.status),
        )?;
        self.recording.log(root.clone() / "metadata", &rerun::TextLog::new(format!(
            "topic={} schema_id={} source_id={} seq={} body_stamp_ns={} header_stamp_ns={} received_ns={} publisher={} observer_seq={} observer_dropped_total={} capability_flags={:#x}",
            observation.topic, observation.schema_id, observation.source_id, observation.seq, observation.body_stamp_ns,
            observation.header_stamp_ns, observation.received_ns, observation.publisher_instance, observation.observer_seq, observation.observer_dropped, observation.capability_flags,
        )))?;
        self.log_media(root, observation.media)?;
        self.recording.flush_with_timeout(FLUSH_TIMEOUT)?;
        Ok(())
    }

    fn log_media(
        &self,
        root: rerun::EntityPath,
        media: Option<Media>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        match media {
            Some(Media::Image(image)) => {
                let resolution = [image.width, image.height];
                match image.pixels {
                    Pixels::Rgb(bytes) => self
                        .recording
                        .log(root / "image", &rerun::Image::from_rgb24(bytes, resolution))?,
                    Pixels::Mono(bytes) => self
                        .recording
                        .log(root / "image", &rerun::Image::from_l8(bytes, resolution))?,
                    Pixels::Depth(values) => {
                        // Rerun 原始 U16 buffer 使用宿主端序；输入解码已按 wire 端序转换。
                        let bytes: Vec<u8> =
                            values.into_iter().flat_map(u16::to_ne_bytes).collect();
                        self.recording.log(
                            root / "image",
                            &rerun::DepthImage::from_gray16(bytes, resolution),
                        )?;
                    }
                }
            }
            Some(Media::PointCloud(cloud)) => {
                self.recording
                    .log(root.clone() / "points", &rerun::Points3D::new(cloud.points))?;
                if let Some(intensity) = cloud.intensity {
                    let shape = vec![intensity.len() as u64];
                    let range = color_range(intensity.iter().copied().map(f64::from));
                    self.log_color_range(&root, range)?;
                    self.recording.log(
                        root / "intensity",
                        &rerun::Tensor::new(TensorData::new(
                            shape,
                            TensorBuffer::F32(intensity.into()),
                        ))
                        .with_value_range(range.bounds),
                    )?;
                }
            }
            Some(Media::Tensor(tensor)) => {
                let range = color_range(tensor.values.iter().copied());
                self.log_color_range(&root, range)?;
                self.recording.log(
                    root / "tensor",
                    &rerun::Tensor::new(TensorData::new(
                        tensor.shape,
                        TensorBuffer::F64(tensor.values.into()),
                    ))
                    .with_value_range(range.bounds),
                )?;
            }
            None => {}
        }
        Ok(())
    }

    fn log_color_range(
        &self,
        root: &rerun::EntityPath,
        range: ColorRange,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let preview = if range.gpu_preview_limit {
            "GPU_PREVIEW_LIMIT"
        } else {
            "GPU_PREVIEW_RANGE_OK"
        };
        self.recording.log(
            root.clone() / "color_range",
            &rerun::TextLog::new(format!(
                "{preview} PER_SAMPLE_COLOR_RANGE min={} max={} STORED_VALUES_UNCHANGED",
                range.bounds[0], range.bounds[1]
            )),
        )?;
        Ok(())
    }

    pub fn health(
        &mut self,
        now_ns: i64,
        statuses: &[(String, String)],
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.flush_blueprint()?;
        self.recording.disable_timeline("source_seq");
        self.recording.disable_timeline("published_time");
        self.recording.disable_timeline("received_time");
        self.recording.set_time(
            "robot_time",
            rerun::external::re_log_types::TimeCell::from_duration_nanos(now_ns),
        );
        for (topic, status) in statuses {
            self.recording.log(
                root(topic) / "freshness",
                &rerun::TextLog::new(status.clone()),
            )?;
        }
        self.recording.flush_with_timeout(FLUSH_TIMEOUT)?;
        Ok(())
    }

    pub fn finish(self) -> Result<(), Box<dyn std::error::Error>> {
        self.recording.flush_with_timeout(FLUSH_TIMEOUT)?;
        self.recording.finalize_deferred_sinks();
        Ok(())
    }

    pub(crate) fn forget(&mut self, topic: &str) {
        self.blueprint_dirty |= self.views.remove(topic).is_some();
    }

    pub(crate) fn retain_instances<'a>(
        &mut self,
        topic: &str,
        active: impl Iterator<Item = &'a str>,
    ) {
        if let Some(source) = self.views.get_mut(topic) {
            // 与运行时相同的 8 实例窗口；不能让显示过滤器永久保存历史来源。
            let names: Vec<_> = active.take(MAX_VIEW_INSTANCES).collect();
            let previous = source.instances.len();
            source
                .instances
                .retain(|id, _| names.contains(&id.as_str()));
            self.blueprint_dirty |= previous != source.instances.len();
        }
    }

    fn register_view(
        &mut self,
        observation: &Observation,
        root: rerun::EntityPath,
    ) -> Result<(), crate::VizError> {
        if !self.views.contains_key(&observation.topic) && self.views.len() >= MAX_VIEW_TOPICS {
            return Err(crate::invalid("视图 topic 超过 64 个活跃来源"));
        }
        let source = self
            .views
            .entry(observation.topic.clone())
            .or_insert_with(|| SourceViews {
                schema: observation.schema_id,
                instances: BTreeMap::new(),
            });
        if source.schema != observation.schema_id {
            return Err(crate::invalid("视图 topic schema 发生冲突"));
        }
        if !source
            .instances
            .contains_key(&observation.publisher_instance)
        {
            if source.instances.len() >= MAX_VIEW_INSTANCES {
                return Err(crate::invalid("视图 topic 超过 8 个活跃实例"));
            }
            source
                .instances
                .insert(observation.publisher_instance.clone(), root);
            self.blueprint_dirty = true;
        }
        Ok(())
    }

    fn flush_blueprint(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if self.blueprint_dirty {
            self.recording.send_blueprint_opts(&BlueprintOpts {
                blueprint: blueprint(&self.views),
                activation: rerun::blueprint::BlueprintActivation::default(),
            })?;
            self.blueprint_dirty = false;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
struct ColorRange {
    bounds: [f64; 2],
    gpu_preview_limit: bool,
}

// 精确相等识别常量；有意模拟 Viewer 的 f32 转换来检测预览损失。
#[allow(clippy::float_cmp, clippy::cast_possible_truncation)]
fn color_range(values: impl Iterator<Item = f64>) -> ColorRange {
    // 输入已验证有限；显式逐帧范围避免 SDK 把一般正值自动当成 0..255 图像。
    let (mut min, mut max) = values
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(min, max), value| {
            (min.min(value), max.max(value))
        });
    if !min.is_finite() || !max.is_finite() {
        return ColorRange {
            bounds: [0.0, 1.0],
            gpu_preview_limit: true,
        };
    }
    if min == max {
        let pad = (min.abs() * 1e-6).max(1.0);
        min = (min - pad).max(-f64::MAX);
        max = (max + pad).min(f64::MAX);
    }
    // Viewer 将数值和范围上传为 f32；不改原值，只明确无法可靠预览的边界。
    let lower = min as f32;
    let upper = max as f32;
    let width = upper - lower;
    ColorRange {
        bounds: [min, max],
        gpu_preview_limit: !lower.is_finite()
            || !upper.is_finite()
            || !width.is_finite()
            || width <= 0.0,
    }
}

fn builder() -> rerun::RecordingStreamBuilder {
    rerun::RecordingStreamBuilder::new("robot-viz")
        .enabled(true)
        .with_blueprint(blueprint(&BTreeMap::new()))
}

fn blueprint(views: &Views) -> Blueprint {
    let curves = curve_tabs(views);
    let bridge = TextLogView::new("Bridge - OBSERVATION_ONLY")
        .with_origin("topics/_bridge")
        .with_contents(["+ $origin/**"]);
    let events = TextLogView::new("Source status and events")
        .with_origin("topics")
        .with_contents(["+ $origin/**", "- $origin/_bridge/**"]);
    // 各相机和点云使用独立视图，不能重叠图像或暗示共享外参。
    let media_views: Vec<ContainerLike> = views
        .iter()
        .flat_map(|(topic, source)| {
            source
                .instances
                .values()
                .map(move |origin| (topic, source.schema, origin))
        })
        .enumerate()
        .filter_map(|(index, (topic, schema, origin))| {
            let name = format!("{index:02} {}", media_name(topic, schema));
            match schema {
                robot_msg::schema_id::SCHEMA_IMAGE_HEADER => Some(
                    Spatial2DView::new(name)
                        .with_origin(origin.clone() / "image")
                        .with_contents(["+ $origin/**"])
                        .into(),
                ),
                robot_msg::schema_id::SCHEMA_POINTCLOUD_HEADER => Some(
                    Spatial3DView::new(name)
                        .with_origin(origin.clone() / "points")
                        .with_contents(["+ $origin/**"])
                        .into(),
                ),
                robot_msg::schema_id::SCHEMA_TENSOR_HEADER => Some(
                    TensorView::new(name)
                        .with_origin(origin.clone() / "tensor")
                        .with_contents(["+ $origin/**"])
                        .into(),
                ),
                _ => None,
            }
        })
        .collect();
    let status = Tabs::new([bridge.into(), events.into()]).with_name("Status");
    let mut children: Vec<ContainerLike> = vec![curves.into(), status.into()];
    if !media_views.is_empty() {
        children.push(
            Tabs::new(media_views)
                .with_name("Media - independent sources")
                .into(),
        );
    }
    Blueprint::new(Grid::new(children).with_grid_columns(2))
        .with_auto_layout(false)
        .with_auto_views(false)
        .with_time_panel(TimePanel::new().with_timeline("robot_time"))
}

fn media_name(topic: &str, schema: u32) -> String {
    let parts: Vec<_> = topic.rsplit('/').take(2).collect();
    let short = parts.into_iter().rev().collect::<Vec<_>>().join("/");
    // Viewer 默认字体不包含中文；完整原始来源保存在 metadata 中。
    let short: String = short
        .chars()
        .map(|ch| if ch.is_ascii() { ch } else { '?' })
        .collect();
    format!("{schema:#x} {short}")
}

fn curve_tabs(views: &Views) -> Tabs {
    use robot_msg::schema_id::{
        SCHEMA_ACTION_EVENT_ENVELOPE, SCHEMA_HEARTBEAT, SCHEMA_JOINT_COMMAND, SCHEMA_JOINT_STATE,
        SCHEMA_TWIST,
    };
    let groups = [
        ("Position (rad)", SCHEMA_JOINT_STATE, "position_rad"),
        ("Velocity (wire)", SCHEMA_JOINT_STATE, "velocity_wire"),
        ("Effort (wire)", SCHEMA_JOINT_STATE, "effort_wire"),
        ("State codes", SCHEMA_JOINT_STATE, "state_code"),
        ("Targets (wire)", SCHEMA_JOINT_COMMAND, "target"),
        ("Twist linear", SCHEMA_TWIST, "linear"),
        ("Twist angular", SCHEMA_TWIST, "angular"),
        ("Heartbeat (Hz)", SCHEMA_HEARTBEAT, "tick_rate_hz"),
        ("Action progress", SCHEMA_ACTION_EVENT_ENVELOPE, "progress"),
    ];
    let children: Vec<ContainerLike> = groups
        .into_iter()
        .map(|(name, schema, field)| {
            TimeSeriesView::new(name)
                .with_origin("topics")
                .with_contents(curve_queries(views, schema, field))
                .into()
        })
        .collect();
    let active = match &children[0] {
        ContainerLike::View(view) => view.blueprint_path().to_string(),
        _ => unreachable!("首个曲线项固定为 View"),
    };
    Tabs::new(children)
        .with_name("Curves by quantity")
        .with_active_tab(active)
}

fn curve_queries(views: &Views, schema: u32, field: &'static str) -> Vec<String> {
    // 本版 SDK 仅支持末尾 /**；必须展开来源路径，不能使用中间通配符。
    let mut queries = vec!["- /topics/**".to_owned()];
    for source in views.values() {
        if source.schema == schema {
            for origin in source.instances.values() {
                queries.push(format!("+ {}/**", origin.clone() / field));
            }
        }
    }
    queries
}

fn root(topic: &str) -> rerun::EntityPath {
    topic
        .trim_start_matches('/')
        .split('/')
        .fold(rerun::EntityPath::from("topics"), |parent, child| {
            parent / EntityPathPart::from(child.to_owned())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rerun::external::re_log_types::LogMsg;

    #[test]
    fn quantity_filters_exclude_other_units_using_official_path_parser() {
        use rerun::external::re_log_types::EntityPathFilter;
        let origin = root("/test/s0/arm/state") / "instances" / "example";
        let schema = robot_msg::schema_id::SCHEMA_JOINT_STATE;
        let views = BTreeMap::from([(
            "/test/s0/arm/state".into(),
            SourceViews {
                schema,
                instances: BTreeMap::from([("example".into(), origin.clone())]),
            },
        )]);
        let query = curve_queries(&views, schema, "position_rad").join("\n");
        let filter = EntityPathFilter::parse_strict(&query)
            .unwrap()
            .resolve_without_substitutions();
        assert!(filter.matches(&(origin.clone() / "position_rad" / "joint_a")));
        assert!(!filter.matches(&(origin.clone() / "effort_wire" / "joint_a")));
        assert!(!filter.matches(&(origin / "state_code" / "mode")));
        let empty = curve_queries(&BTreeMap::new(), schema, "position_rad").join("\n");
        assert!(
            !EntityPathFilter::parse_strict(&empty)
                .unwrap()
                .resolve_without_substitutions()
                .matches(&root("/unknown/position_rad"))
        );
    }

    fn sample(instance: &str) -> Observation {
        Observation {
            topic: "/test/s0/arm/state".into(),
            publisher_instance: instance.into(),
            observer_seq: 1,
            observer_dropped: 0,
            schema_id: robot_msg::schema_id::SCHEMA_JOINT_STATE,
            source_id: 7,
            capability_flags: 0,
            seq: 1,
            header_stamp_ns: 2,
            body_stamp_ns: 1,
            received_ns: 3,
            series: vec![crate::decode::Series {
                path: vec!["position_rad".into(), "joint_a".into()],
                value: 0.125,
            }],
            status: "OBSERVATION_ONLY".into(),
            media: None,
        }
    }

    #[test]
    fn bounded_views_keep_both_instances_without_repeated_blueprints() {
        let (recording, storage) = rerun::RecordingStreamBuilder::new("instance-view-test")
            .memory()
            .unwrap();
        let mut output = Output::from_recording(recording).unwrap();
        storage.take();
        output.log(sample("a")).unwrap();
        output.log(sample("b")).unwrap();
        let initial = storage.take();
        assert_eq!(
            initial
                .iter()
                .filter(|message| matches!(message, LogMsg::BlueprintActivationCommand(_)))
                .count(),
            2
        );
        let queries = curve_queries(
            &output.views,
            robot_msg::schema_id::SCHEMA_JOINT_STATE,
            "position_rad",
        );
        assert_eq!(queries.len(), 3);
        assert!(
            queries
                .iter()
                .any(|query| query.ends_with("/instances/a/position_rad/**"))
        );
        assert!(
            queries
                .iter()
                .any(|query| query.ends_with("/instances/b/position_rad/**"))
        );
        for id in ["a", "b", "a", "b"] {
            output.log(sample(id)).unwrap();
        }
        assert!(
            !storage
                .take()
                .iter()
                .any(|message| matches!(message, LogMsg::BlueprintActivationCommand(_)))
        );
        let topic = "/test/s0/arm/state";
        for id in 2..8 {
            let observation = sample(&id.to_string());
            output
                .register_view(
                    &observation,
                    root(topic)
                        / "instances"
                        / EntityPathPart::from(observation.publisher_instance.clone()),
                )
                .unwrap();
        }
        assert!(output.register_view(&sample("ninth"), root(topic)).is_err());
        assert_eq!(output.views[topic].instances.len(), 8);
        output.retain_instances(topic, ["b"].into_iter());
        output.health(4, &[]).unwrap();
        assert_eq!(
            output.views[topic].instances.keys().collect::<Vec<_>>(),
            vec!["b"]
        );
        assert_eq!(
            storage
                .take()
                .iter()
                .filter(|message| matches!(message, LogMsg::BlueprintActivationCommand(_)))
                .count(),
            1
        );
        for index in 1..64 {
            let mut observation = sample("a");
            observation.topic = format!("/test/{index}");
            output
                .register_view(&observation, root(&observation.topic))
                .unwrap();
        }
        let mut extra = sample("a");
        extra.topic = "/test/overflow".into();
        assert!(output.register_view(&extra, root(&extra.topic)).is_err());
        assert_eq!(output.views.len(), 64);
        output.forget(topic);
        assert!(!output.views.contains_key(topic));
        output.register_view(&extra, root(&extra.topic)).unwrap();
        assert_eq!(output.views.len(), 64);
    }

    #[test]
    fn color_ranges_cover_actual_values_and_mark_gpu_boundaries() {
        let range = color_range([0.5, 1.5, 2.5, 3.5].into_iter());
        assert_eq!(range.bounds, [0.5, 3.5]);
        assert!(!range.gpu_preview_limit);
        for value in [0.0, 3.5, -3.5, 1e20] {
            let range = color_range([value, value].into_iter());
            assert!(range.bounds[0] < value && range.bounds[1] > value);
            assert!(!range.gpu_preview_limit);
        }
        for values in [
            [1.0, 1.0 + f64::EPSILON],
            [f64::MAX, f64::MAX],
            [-f64::MAX, f64::MAX],
            [1e-300, 2e-300],
        ] {
            let range = color_range(values.into_iter());
            assert!(range.bounds.iter().all(|value| value.is_finite()));
            assert!(range.bounds[0] < range.bounds[1]);
            assert!(range.gpu_preview_limit);
        }
    }

    #[test]
    fn official_sdk_records_explicit_range_and_keeps_extreme_values() {
        let (recording, storage) = rerun::RecordingStreamBuilder::new("tensor-range-test")
            .memory()
            .unwrap();
        let mut output = Output::from_recording(recording).unwrap();
        let mut observation = sample("tensor");
        observation.schema_id = robot_msg::schema_id::SCHEMA_TENSOR_HEADER;
        observation.series.clear();
        observation.media = Some(Media::Tensor(crate::media::Tensor {
            shape: vec![2],
            values: vec![1e100, 2e100],
        }));
        output.log(observation).unwrap();
        let mut ranges = Vec::new();
        let mut tensors = Vec::new();
        let mut texts = Vec::new();
        for message in storage.take() {
            if let LogMsg::ArrowMsg(_, message) = message {
                let chunk = rerun::log::Chunk::from_arrow_msg(&message).unwrap();
                for descriptor in chunk.component_descriptors() {
                    match descriptor
                        .component_type
                        .map(|value| value.to_string())
                        .as_deref()
                    {
                        Some("rerun.components.ValueRange") => {
                            for row in chunk.iter_component::<rerun::components::ValueRange>(
                                descriptor.component,
                            ) {
                                ranges.extend(row.as_slice().iter().map(|range| range.0.0));
                            }
                        }
                        Some("rerun.components.TensorData") => {
                            for row in chunk.iter_component::<rerun::components::TensorData>(
                                descriptor.component,
                            ) {
                                tensors
                                    .extend(row.as_slice().iter().map(|tensor| tensor.0.clone()));
                            }
                        }
                        Some("rerun.components.Text") => {
                            for row in chunk
                                .iter_component::<rerun::components::Text>(descriptor.component)
                            {
                                texts
                                    .extend(row.as_slice().iter().map(|value| value.0.to_string()));
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        assert_eq!(ranges, vec![[1e100, 2e100]]);
        assert!(
            texts
                .iter()
                .any(|text| text.contains("GPU_PREVIEW_LIMIT PER_SAMPLE_COLOR_RANGE"))
        );
        assert_eq!(tensors.len(), 1);
        match &tensors[0].buffer {
            TensorBuffer::F64(values) => assert_eq!(values.as_ref(), &[1e100, 2e100]),
            other => panic!("原始数值的 F64 dtype 被改变: {other:?}"),
        }
    }

    #[test]
    fn official_sdk_preserves_integer_sequence_and_both_nanosecond_stamps() {
        let (recording, storage) = rerun::RecordingStreamBuilder::new("integer-timeline-test")
            .memory()
            .unwrap();
        let mut output = Output::from_recording(recording).unwrap();
        let value = 9_007_199_254_740_999_i64;
        output
            .log(Observation {
                topic: "/test/s0/arm/state".into(),
                publisher_instance: "instance".into(),
                observer_seq: 1,
                observer_dropped: 0,
                schema_id: robot_msg::schema_id::SCHEMA_JOINT_STATE,
                source_id: 7,
                capability_flags: 0,
                seq: value,
                header_stamp_ns: value + 1,
                body_stamp_ns: value - 1,
                received_ns: value + 2,
                series: Vec::new(),
                status: "整数不能经过 f64 时间轴".into(),
                media: None,
            })
            .unwrap();
        let mut values: BTreeMap<String, Vec<i64>> = BTreeMap::new();
        for message in storage.take() {
            if let LogMsg::ArrowMsg(_, message) = message {
                let chunk = rerun::log::Chunk::from_arrow_msg(&message).unwrap();
                for column in chunk.timelines().values() {
                    values
                        .entry(column.name().to_string())
                        .or_default()
                        .extend_from_slice(column.times_raw());
                }
            }
        }
        assert!(values["source_seq"].contains(&value));
        assert!(values["robot_time"].contains(&(value - 1)));
        assert!(values["published_time"].contains(&(value + 1)));
        assert!(values["received_time"].contains(&(value + 2)));
    }
}
