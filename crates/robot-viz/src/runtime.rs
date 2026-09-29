//! 单 worker 的发现、接收和报告。输出一帧后才取下一帧，不积累媒体队列。

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;

use iceoryx2::prelude::NodeBuilder;
use iceoryx2::service::ipc::Service;
use robot_core::time::{Deadline, DurationNs, RobotClock, SystemClock};
use robot_transport::iceoryx2::{ObserverMetadata, list_observers_filtered};
use serde::Serialize;

use crate::config::Layouts;
use crate::decode::PUBSUB_SCHEMAS;
use crate::input::{self, Source};
use crate::output::Output;
use crate::{invalid, supervisor};

type Error = Box<dyn std::error::Error>;
const MAX_TOPICS: usize = 64;
const HEALTH_INTERVAL_NS: u64 = 250_000_000;
const STALE_NS: i64 = 500_000_000;

/// 命令行配置；save 默认有限录制，connect 默认持续到信号。
#[derive(Debug)]
pub struct Options {
    pub configs: Vec<PathBuf>,
    pub save: Option<PathBuf>,
    pub connect: Option<String>,
    pub duration_ms: Option<u64>,
    pub max_frames: Option<u64>,
    pub report: Option<PathBuf>,
}

impl Options {
    pub fn parse(args: impl IntoIterator<Item = std::ffi::OsString>) -> Result<Self, Error> {
        let mut options = Self {
            configs: Vec::new(),
            save: None,
            connect: None,
            duration_ms: None,
            max_frames: None,
            report: None,
        };
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            let value = args.next().ok_or_else(|| invalid("参数缺少值"))?;
            match arg.to_str() {
                Some("--config") => options.configs.push(PathBuf::from(value)),
                Some("--save") => options.save = Some(PathBuf::from(value)),
                Some("--connect") => {
                    options.connect = Some(
                        value
                            .into_string()
                            .map_err(|_| invalid("连接地址不是 UTF-8"))?,
                    );
                }
                Some("--duration-ms") => options.duration_ms = Some(number(&value)?),
                Some("--max-frames") => options.max_frames = Some(number(&value)?),
                Some("--report") => options.report = Some(PathBuf::from(value)),
                _ => return Err(invalid("未知参数；使用 --help").into()),
            }
        }
        if options.configs.is_empty()
            || options.configs.len() > 16
            || options.save.is_some() == options.connect.is_some()
        {
            return Err(invalid("需提供 1..16 个 --config，且 --save/--connect 必须二选一").into());
        }
        if options
            .duration_ms
            .is_some_and(|value| value == 0 || value > 86_400_000)
            || options.max_frames == Some(0)
        {
            return Err(invalid("duration-ms 为 1..86400000，max-frames 为正数").into());
        }
        if options.save.is_some() {
            options.duration_ms.get_or_insert(10_000);
            options.max_frames.get_or_insert(1000);
            if options.max_frames.is_some_and(|value| value > 10_000) {
                return Err(invalid(
                    "单个 RRD 最多 10000 帧，避免 SDK footer 长期积累；长期观察使用 --connect",
                )
                .into());
            }
        }
        Ok(options)
    }
}

fn number(value: &std::ffi::OsStr) -> Result<u64, Error> {
    Ok(value
        .to_str()
        .ok_or_else(|| invalid("数值不是 UTF-8"))?
        .parse()?)
}

#[derive(Debug, Default, Serialize)]
pub struct SchemaCounts {
    received: u64,
    logged: u64,
    rejected: u64,
}

#[derive(Debug, Default, Serialize)]
pub struct Report {
    frames_received: u64,
    frames_logged: u64,
    frames_rejected: u64,
    per_schema: BTreeMap<u32, SchemaCounts>,
    reject_reasons: Vec<String>,
    topics_seen: Vec<String>,
    topics_discovered_total: u64,
    topics_retired: u64,
    observer_dropped: u64,
    observer_sequence_gaps: u64,
    publisher_instances_seen: u64,
    instance_cache_evictions: u64,
    output_status: String,
    error: Option<String>,
}

impl Report {
    fn reject(&mut self, schema: u32, reason: String) {
        self.frames_rejected += 1;
        self.per_schema.entry(schema).or_default().rejected += 1;
        self.reason(reason);
    }

    fn reason(&mut self, reason: String) {
        tracing::warn!(reason, "观测输入被拒绝");
        if self.reject_reasons.len() < 64 && !self.reject_reasons.contains(&reason) {
            self.reject_reasons.push(reason);
        }
    }
}

struct Slot {
    source: Box<dyn Source>,
    schema: u32,
    last_seen_ns: u64,
    instances: BTreeMap<[u8; 16], Instance>,
}

struct Instance {
    last_body: Option<i64>,
    metadata: Option<ObserverMetadata>,
    name: String,
    last_seen_ns: u64,
    last_source_seq: Option<i64>,
}

/// worker 的唯一入口；失败报告先写盘，随后 SDK Drop 由外层监督。
pub fn run(options: &Options) -> Result<(), Error> {
    let mut report = Report::default();
    let result = run_inner(options, &mut report);
    match &result {
        Ok(()) => report.output_status = "success".into(),
        Err(error) => {
            report.output_status = "error".into();
            report.error = Some(error.to_string());
        }
    }
    if let Some(path) = &options.report {
        std::fs::write(path, serde_json::to_vec_pretty(&report)?)?;
    }
    result
}

fn run_inner(options: &Options, report: &mut Report) -> Result<(), Error> {
    let mut layouts = Layouts::default();
    for path in &options.configs {
        layouts.load(path)?;
    }
    let clock = SystemClock;
    supervisor::progress("starting")?;
    let mut output = match (&options.save, &options.connect) {
        (Some(path), None) => Output::save(path)?,
        (None, Some(url)) => Output::connect(url)?,
        _ => return Err(invalid("输出配置无效").into()),
    };
    let mut config = iceoryx2::config::Config::default();
    config.global.node.cleanup_dead_nodes_on_creation = false;
    config.global.node.cleanup_dead_nodes_on_destruction = false;
    let node = NodeBuilder::new()
        .config(&config)
        .create::<Service>()
        .map_err(|error| invalid(format!("observer node: {error:?}")))?;
    let mut slots = BTreeMap::<String, Slot>::new();
    let started = clock.now().0;
    let duration_ns = options.duration_ms.map(|value| value * 1_000_000);
    let mut next_health = 0;
    let result: Result<(), Error> = (|| {
        loop {
            let now = clock.now().0;
            if supervisor::stopping()
                || duration_ns.is_some_and(|limit| now.saturating_sub(started) >= limit)
                || limit_reached(options, report)
            {
                break;
            }
            if now >= next_health {
                supervisor::progress("discovering")?;
                discover(&node, &layouts, &mut slots, report, &mut output, now)?;
                supervisor::progress("flushing")?;
                output.health(i64::try_from(now)?, &health(&slots, report, now))?;
                next_health = now.saturating_add(HEALTH_INTERVAL_NS);
            }
            for (topic, slot) in &mut slots {
                if supervisor::stopping() || limit_reached(options, report) {
                    break;
                }
                process_slot(topic, slot, &mut layouts, report, &mut output, clock)?;
            }
            supervisor::progress("running")?;
            clock.sleep_until(Deadline::from_now_plus(
                clock.now(),
                DurationNs::from_millis(10),
            ));
        }
        Ok(())
    })();
    if let Err(error) = &result {
        report.output_status = "error".into();
        report.error = Some(error.to_string());
        if let Some(path) = &options.report {
            std::fs::write(path, serde_json::to_vec_pretty(&report)?)?;
        }
    }
    supervisor::progress("stopping")?;
    drop(slots);
    result?;
    output.finish()?;
    Ok(())
}

fn limit_reached(options: &Options, report: &Report) -> bool {
    options
        .max_frames
        .is_some_and(|limit| report.frames_received >= limit)
}

fn discover(
    node: &iceoryx2::prelude::Node<Service>,
    layouts: &Layouts,
    slots: &mut BTreeMap<String, Slot>,
    report: &mut Report,
    output: &mut Output,
    now: u64,
) -> Result<(), Error> {
    // 无帧不等于任务结束。只回收观察订阅，下一轮仍可重新发现同一来源。
    let expired: Vec<_> = slots
        .iter()
        .filter(|(_, slot)| now.saturating_sub(slot.last_seen_ns) >= 5_000_000_000)
        .map(|(topic, _)| topic.clone())
        .collect();
    for topic in expired {
        slots.remove(&topic);
        report.topics_retired += 1;
        report.reason(format!(
            "镜像连续 5 秒无帧，释放观察订阅：{topic}；来源状态未知"
        ));
        output.forget(&topic);
    }
    let discovered = list_observers_filtered(
        node.config(),
        MAX_TOPICS.saturating_sub(slots.len()),
        |topic| layouts.allows_topic(topic) && !slots.contains_key(topic),
    )?;
    if discovered.capacity_reached {
        report.reason("同时观测 topic 已达 64 个上限".into());
    }
    for topic in discovered.topics {
        if !PUBSUB_SCHEMAS.contains(&topic.schema_id) {
            report.reason(format!("拒绝非 pub/sub schema {}", topic.schema_id));
            continue;
        }
        match input::open(node, &topic.primary_topic, topic.schema_id) {
            Ok(source) => {
                report.topics_discovered_total += 1;
                if report.topics_seen.len() == MAX_TOPICS {
                    report.topics_seen.remove(0);
                }
                report.topics_seen.push(topic.primary_topic.clone());
                slots.insert(
                    topic.primary_topic,
                    Slot {
                        source,
                        schema: topic.schema_id,
                        last_seen_ns: now,
                        instances: BTreeMap::new(),
                    },
                );
            }
            Err(error) => report.reason(error.to_string()),
        }
    }
    Ok(())
}

fn health(slots: &BTreeMap<String, Slot>, report: &Report, now: u64) -> Vec<(String, String)> {
    let mut result = vec![(
        "_bridge".into(),
        format!(
            "OBSERVATION_ONLY sources={} logged={} rejected={} dropped_window={} seq_gaps={} retired={} state={} UNKNOWN_NOT_SAFE",
            slots.len(),
            report.frames_logged,
            report.frames_rejected,
            report.observer_dropped,
            report.observer_sequence_gaps,
            report.topics_retired,
            if slots.is_empty() {
                "NO_DATA"
            } else {
                "OBSERVING"
            },
        ),
    )];
    for (topic, slot) in slots {
        if slot.instances.is_empty() {
            result.push((topic.clone(), "NO_DATA UNKNOWN_NOT_SAFE".into()));
        }
        for instance in slot.instances.values() {
            let status = match instance.last_body {
                None => "NO_DATA UNKNOWN_NOT_SAFE".to_owned(),
                Some(stamp) => {
                    let age = i128::from(now) - i128::from(stamp);
                    let freshness = freshness(Some(stamp), now);
                    format!(
                        "{freshness} body_stamp_ns={stamp} age_ns={age} publisher={}",
                        instance.name
                    )
                }
            };
            result.push((format!("{topic}/instances/{}", instance.name), status));
        }
    }
    result
}

fn freshness(stamp: Option<i64>, now: u64) -> &'static str {
    match stamp {
        None => "NO_DATA UNKNOWN_NOT_SAFE",
        Some(stamp) if i128::from(now) - i128::from(stamp) > i128::from(STALE_NS) => {
            "STALE UNKNOWN_NOT_SAFE"
        }
        Some(_) => "RECENT_SAMPLE OBSERVATION_ONLY UNKNOWN_NOT_SAFE",
    }
}

fn process_slot(
    topic: &str,
    slot: &mut Slot,
    layouts: &mut Layouts,
    report: &mut Report,
    output: &mut Output,
    clock: SystemClock,
) -> Result<(), Error> {
    let received = match slot.source.poll(topic, layouts) {
        Ok(value) => value,
        Err(error) => {
            report.reject(slot.schema, error.to_string());
            return Ok(());
        }
    };
    let Some((metadata, observation)) = received else {
        return Ok(());
    };
    slot.last_seen_ns = clock.now().0;
    report.frames_received += 1;
    report.per_schema.entry(slot.schema).or_default().received += 1;
    let instance = match track_instance(&mut slot.instances, metadata, report, slot.last_seen_ns) {
        Ok(value) => value,
        Err(error) => {
            report.reject(slot.schema, error.to_string());
            return Ok(());
        }
    };
    match observation {
        Ok(mut observation) => {
            // 原 seq 可以重复（如 EStop）；同实例倒退不能被新实例掩盖。
            if instance
                .last_source_seq
                .is_some_and(|previous| observation.seq < previous)
            {
                report.reject(slot.schema, "同 publisher 实例原始 seq 倒退".into());
                return Ok(());
            }
            instance.last_source_seq = Some(observation.seq);
            instance.last_body = Some(observation.body_stamp_ns);
            observation.publisher_instance.clone_from(&instance.name);
            observation.observer_seq = metadata.observer_seq;
            observation.observer_dropped = metadata.dropped_frames;
            output.retain_instances(
                topic,
                slot.instances.values().map(|value| value.name.as_str()),
            );
            supervisor::progress("flushing")?;
            output.log(observation)?;
            report.frames_logged += 1;
            report.per_schema.entry(slot.schema).or_default().logged += 1;
        }
        Err(error) => report.reject(slot.schema, error.to_string()),
    }
    // 错误帧也可能触发实例窗口淘汰，视图不能保留已淘汰实例。
    output.retain_instances(
        topic,
        slot.instances.values().map(|value| value.name.as_str()),
    );
    Ok(())
}

fn track_instance<'a>(
    instances: &'a mut BTreeMap<[u8; 16], Instance>,
    metadata: ObserverMetadata,
    report: &mut Report,
    now: u64,
) -> Result<&'a mut Instance, crate::VizError> {
    if !instances.contains_key(&metadata.publisher_instance_id) {
        if instances.len() >= 8 {
            let expired = instances
                .iter()
                .filter(|(_, value)| now.saturating_sub(value.last_seen_ns) >= 5_000_000_000)
                .min_by_key(|(_, value)| value.last_seen_ns)
                .map(|(id, _)| *id);
            let Some(id) = expired else {
                return Err(invalid("同 topic 已有 8 个活跃实例"));
            };
            instances.remove(&id);
            report.instance_cache_evictions += 1;
            report.reason("实例缓存淘汰超过 5 秒未见的数据源；重新发现开启新的计数窗口".into());
        }
        // 此计数指缓存窗口的新建次数，并不宣称跨淘汰的全生命周期去重。
        report.publisher_instances_seen += 1;
        let mut name = String::with_capacity(32);
        for byte in metadata.publisher_instance_id {
            let _ = write!(&mut name, "{byte:02x}");
        }
        instances.insert(
            metadata.publisher_instance_id,
            Instance {
                last_body: None,
                metadata: None,
                last_source_seq: None,
                last_seen_ns: now,
                name,
            },
        );
    }
    let instance = instances
        .get_mut(&metadata.publisher_instance_id)
        .ok_or_else(|| invalid("实例缓存内部不一致"))?;
    if let Some(old) = instance.metadata {
        if metadata.observer_seq <= old.observer_seq || metadata.dropped_frames < old.dropped_frames
        {
            return Err(invalid("同 publisher 实例镜像序号重复/倒退或丢帧计数倒退"));
        }
        report.observer_dropped += metadata.dropped_frames - old.dropped_frames;
        report.observer_sequence_gaps += metadata
            .observer_seq
            .saturating_sub(old.observer_seq.saturating_add(1));
    }
    // attach 前累计丢帧只保留原始 metadata，不重复计入本观察窗口。
    instance.metadata = Some(metadata);
    instance.last_seen_ns = now;
    Ok(instance)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readable_health_becomes_stale_without_any_new_frame() {
        assert_eq!(freshness(None, 100), "NO_DATA UNKNOWN_NOT_SAFE");
        assert!(freshness(Some(1), 500_000_001).contains("RECENT_SAMPLE"));
        assert_eq!(freshness(Some(1), 500_000_002), "STALE UNKNOWN_NOT_SAFE");
        assert!(
            health(&BTreeMap::new(), &Report::default(), 1)[0]
                .1
                .is_ascii()
        );
    }

    fn metadata(id: u8, seq: u64, dropped: u64) -> ObserverMetadata {
        ObserverMetadata {
            publisher_instance_id: [id; 16],
            observer_seq: seq,
            dropped_frames: dropped,
        }
    }

    #[test]
    fn concurrent_instances_keep_independent_counters_and_reject_regression() {
        let mut instances = BTreeMap::new();
        let mut report = Report::default();
        track_instance(&mut instances, metadata(1, 10, 100), &mut report, 1).unwrap();
        track_instance(&mut instances, metadata(2, 1, 900), &mut report, 2).unwrap();
        track_instance(&mut instances, metadata(1, 13, 102), &mut report, 3).unwrap();
        track_instance(&mut instances, metadata(2, 2, 901), &mut report, 4).unwrap();
        assert_eq!(report.publisher_instances_seen, 2);
        assert_eq!(report.observer_dropped, 3);
        assert_eq!(report.observer_sequence_gaps, 2);
        assert!(track_instance(&mut instances, metadata(1, 13, 102), &mut report, 5).is_err());
        assert!(track_instance(&mut instances, metadata(1, 14, 101), &mut report, 5).is_err());
        assert_eq!(instances[&[1; 16]].metadata, Some(metadata(1, 13, 102)));
    }

    #[test]
    fn instance_capacity_is_bounded_and_expired_reentry_does_not_recount_old_drops() {
        let mut instances = BTreeMap::new();
        let mut report = Report::default();
        for id in 0..8 {
            track_instance(&mut instances, metadata(id, 1, 100), &mut report, 1).unwrap();
        }
        assert!(track_instance(&mut instances, metadata(9, 1, 100), &mut report, 2).is_err());
        track_instance(
            &mut instances,
            metadata(9, 1, 100),
            &mut report,
            5_000_000_001,
        )
        .unwrap();
        assert_eq!(instances.len(), 8);
        assert_eq!(report.instance_cache_evictions, 1);
        assert_eq!(report.observer_dropped, 0);
    }

    #[test]
    fn live_is_unbounded_by_default_and_file_has_explicit_limits() {
        let args = [
            "--config",
            "robot.toml",
            "--connect",
            "rerun+http://127.0.0.1:9876/proxy",
        ]
        .map(Into::into);
        let live = Options::parse(args).unwrap();
        assert_eq!((live.duration_ms, live.max_frames), (None, None));
        let args = ["--config", "robot.toml", "--save", "output.rrd"].map(Into::into);
        let save = Options::parse(args).unwrap();
        assert_eq!(
            (save.duration_ms, save.max_frames),
            (Some(10_000), Some(1000))
        );
        let empty = health(&BTreeMap::new(), &Report::default(), 1);
        assert!(empty[0].1.contains("NO_DATA"));
    }
}
