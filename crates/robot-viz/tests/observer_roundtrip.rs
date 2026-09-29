//! 显式软件验收：真实共享内存 → 独立只读桥进程 → 官方 RRD 解码。
//! 不启动 S0/S1、不使用真实 `robot_id`、不访问硬件。

#![cfg(unix)]

#[path = "support/fixtures.rs"]
mod fixtures;

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::net::TcpListener;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use iceoryx2::prelude::NodeBuilder;
use iceoryx2::service::ipc::Service;
use rerun::external::re_log_encoding::DecoderApp;
use rerun::external::re_log_types::{LogMsg, StoreKind};
use robot_core::time::{DurationNs, RobotClock, SystemClock};
use robot_msg::schema_id::{
    SCHEMA_ESTOP_EVENT, SCHEMA_IMAGE_HEADER, SCHEMA_JOINT_LAYOUT, SCHEMA_JOINT_STATE,
};
use serde_json::Value;

static NEXT_CASE: AtomicU64 = AtomicU64::new(1);
// 两个场景共用进程级镜像 SHM 预算，串行保持每次媒体夹具独立。
static CASE_LOCK: Mutex<()> = Mutex::new(());

struct Evidence {
    root: PathBuf,
    _temporary: Option<tempfile::TempDir>,
}

impl Evidence {
    fn new(robot_id: &str) -> Self {
        if let Some(parent) = std::env::var_os("ROBOT_VIZ_TEST_ARTIFACTS") {
            let root = PathBuf::from(parent).join(robot_id);
            fs::create_dir_all(&root).expect("建立本次软件验收证据目录");
            Self {
                root,
                _temporary: None,
            }
        } else {
            let temporary = tempfile::tempdir().expect("测试临时目录");
            Self {
                root: temporary.path().to_path_buf(),
                _temporary: Some(temporary),
            }
        }
    }
}

struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn child(&mut self) -> &mut Child {
        self.0.as_mut().expect("桥子进程仍由测试持有")
    }

    fn wait(&mut self) -> ExitStatus {
        let status = self.child().wait().expect("真实回收桥子进程");
        self.0 = None;
        status
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            if child.try_wait().ok().flatten().is_none() {
                let group = i32::try_from(child.id()).expect("自有测试进程组 ID");
                // SAFETY：spawn 时新建自有进程组；先允许桥父进程收回其 SDK worker。
                unsafe {
                    libc::kill(-group, libc::SIGTERM);
                }
                let clock = SystemClock::new();
                let deadline = clock.now().0 + 12_000_000_000;
                while child.try_wait().ok().flatten().is_none() && clock.now().0 < deadline {
                    clock.sleep_for(DurationNs(20_000_000));
                }
                if child.try_wait().ok().flatten().is_none() {
                    // 异常分支只尽力清场，绝不把强杀写作 worker 正常收尾通过。
                    // SAFETY：与上方相同且仍存活的隔离测试进程组。
                    unsafe {
                        libc::kill(-group, libc::SIGKILL);
                    }
                    let _ = child.kill();
                }
            }
            let _ = child.wait();
        }
    }
}

fn unique_robot(clock: &impl RobotClock, label: &str) -> String {
    format!(
        "viz_{label}_{}_{}_{}",
        std::process::id(),
        clock.now().0,
        NEXT_CASE.fetch_add(1, Ordering::Relaxed)
    )
}

#[allow(clippy::too_many_lines)] // 同一自有子进程生命周期内协调四次真实事件记录握手。
fn run_bridge(
    clock: &impl RobotClock,
    evidence: &Evidence,
    robot_id: &str,
    sources: &mut [Box<dyn fixtures::FixtureSource>],
) -> Value {
    let config = evidence.root.join("robot.toml");
    fs::write(&config, fixtures::config(robot_id)).expect("写隔离配置");
    let stderr = File::create(evidence.root.join("bridge.stderr.log")).unwrap();
    let stdout = File::create(evidence.root.join("bridge.stdout.log")).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_robot-viz"))
        .arg("--config")
        .arg(&config)
        .arg("--save")
        .arg(evidence.root.join("observation.rrd"))
        .arg("--report")
        .arg(evidence.root.join("report.json"))
        .args(["--duration-ms", "3000"])
        .env("ROBOT_LOG_FORMAT", "json")
        .env_remove("ROBOT_VIZ_INTERNAL_WORKER")
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .spawn()
        .expect("启动真实 robot-viz 二进制");
    let mut child = ChildGuard(Some(child));
    let event_sources: Vec<_> = sources
        .iter()
        .enumerate()
        .filter_map(|(index, source)| (source.schema() == SCHEMA_ESTOP_EVENT).then_some(index))
        .collect();
    assert!(
        matches!(event_sources.len(), 0 | 2),
        "本夹具的事件握手只接受零个或两个来源"
    );
    let mut event_phase = if event_sources.is_empty() { 4 } else { 0 };
    let mut last_event_sent = [0_u64; 2];
    let mut minimum_event_seq = 1_u64;
    let deadline = clock.now().0 + 35_000_000_000;
    while child.child().try_wait().unwrap().is_none() {
        assert!(
            clock.now().0 < deadline,
            "桥未在监督上限内退出；证据在 {}",
            evidence.root.display()
        );
        // A→B→A→B，每次须由真实 RRD 确认该实例的新序号后才换来源。
        // 这验证实例识别，不把有损镜像过载下的调度公平性混入内容验收。
        for (index, source) in sources.iter_mut().enumerate() {
            if let Some(event) = event_sources.iter().position(|source| *source == index) {
                if event_phase >= 4 || event != event_phase % 2 {
                    continue;
                }
                last_event_sent[event] = source.publish(clock.now().0);
            } else {
                source.publish(clock.now().0);
            }
        }
        clock.sleep_for(DurationNs(20_000_000));
        if event_phase < 4 {
            let recorded = live_recording_events(&evidence.root.join("observation.rrd"), robot_id);
            let event = event_phase % 2;
            if recorded[event] >= minimum_event_seq {
                eprintln!(
                    "事件实例握手：phase={event_phase} source={event} source_seq={} minimum_seq={minimum_event_seq} observed_ns={}",
                    recorded[event],
                    clock.now().0
                );
                event_phase += 1;
                minimum_event_seq = last_event_sent[event_phase % 2] + 1;
            }
        }
    }
    let status = child.wait();
    let stderr = fs::read_to_string(evidence.root.join("bridge.stderr.log")).unwrap();
    assert!(status.success(), "桥退出失败：{status}\n{stderr}");
    assert_eq!(
        event_phase, 4,
        "必须从真实 RRD 确认 A→B→A→B 四次新序号；不可只证明 mirror 发送成功"
    );
    let report: Value =
        serde_json::from_slice(&fs::read(evidence.root.join("report.json")).unwrap()).unwrap();
    assert_eq!(report["output_status"], "success", "{report}");
    assert_eq!(
        report["worker_reaped"], true,
        "父进程须报告 SDK worker 真实回收：{report}"
    );
    eprintln!("软件观测证据：{}", evidence.root.display());
    report
}

/// 仅用于发送阶段握手：读取当前文件长度内完整解码出的事件，允许末尾仍在写。
/// 收尾后的 `decode_recording` 仍须严格解码全部 RRD，不能用此快照代替完整性验收。
fn live_recording_events(path: &Path, robot_id: &str) -> [u64; 2] {
    let mut found = [0; 2];
    let mut recording_ids = Vec::new();
    let Ok(file) = File::open(path) else {
        return found;
    };
    let Ok(metadata) = file.metadata() else {
        return found;
    };
    let Ok(decoder) = DecoderApp::decode_eager(BufReader::new(file.take(metadata.len()))) else {
        return found;
    };
    for message in decoder {
        let Ok(message) = message else {
            break;
        };
        if let LogMsg::SetStoreInfo(info) = &message
            && info.info.store_id.kind() == StoreKind::Recording
        {
            recording_ids.push(info.info.store_id.clone());
        }
        let LogMsg::ArrowMsg(store_id, message) = message else {
            continue;
        };
        let Ok(chunk) = rerun::log::Chunk::from_arrow_msg(&message) else {
            break;
        };
        let entity = chunk.entity_path().to_string();
        if !instance_entity(
            entity.trim_start_matches('/'),
            robot_id,
            "events/estop",
            "status",
        ) {
            continue;
        }
        assert_eq!(
            store_id.kind(),
            StoreKind::Recording,
            "观测事件不能写到 Blueprint store"
        );
        assert!(
            recording_ids.contains(&store_id),
            "事件所属 Recording store 必须有匹配身份"
        );
        let sequence = chunk
            .timelines()
            .values()
            .filter(|column| column.name() == "source_seq")
            .flat_map(|column| column.times_raw().iter().copied())
            .max()
            .and_then(|value| u64::try_from(value).ok())
            .unwrap_or(0);
        for descriptor in chunk.component_descriptors() {
            if descriptor
                .component_type
                .is_none_or(|kind| kind != "rerun.components.Text")
            {
                continue;
            }
            for row in chunk.iter_component::<rerun::components::Text>(descriptor.component) {
                for text in row.as_slice() {
                    let text = text.0.to_string();
                    for (index, reason) in fixtures::ESTOP_REASONS.iter().enumerate() {
                        if text.contains(reason) {
                            found[index] = found[index].max(sequence);
                        }
                    }
                }
            }
        }
    }
    found
}

#[derive(Default)]
struct Recording {
    entities: BTreeSet<String>,
    components: BTreeMap<String, BTreeSet<String>>,
    scalars: BTreeMap<String, Vec<f64>>,
    images: BTreeMap<String, Vec<Vec<u8>>>,
    positions: BTreeMap<String, Vec<[f32; 3]>>,
    tensors: BTreeMap<String, Vec<rerun::encodings::TensorData>>,
    texts: BTreeMap<String, Vec<String>>,
}

#[allow(clippy::too_many_lines)] // 协议清单与所有实际值断言在同一解码通路逐项核对。
fn decode_recording(path: &Path) -> Recording {
    let reader = BufReader::new(File::open(path).expect("RRD 必须实际生成"));
    let decoder = DecoderApp::decode_eager(reader).expect("官方解码器接受 RRD 头");
    let mut recording = Recording::default();
    let mut store_info = 0;
    let mut recording_ids = Vec::new();
    for message in decoder {
        match message.expect("RRD 所有消息必须完整解码") {
            LogMsg::SetStoreInfo(info) => {
                store_info += 1;
                if info.info.store_id.kind() == StoreKind::Recording {
                    recording_ids.push(info.info.store_id);
                }
            }
            LogMsg::ArrowMsg(store_id, message) => {
                let chunk =
                    rerun::log::Chunk::from_arrow_msg(&message).expect("有效的 Rerun 数据块");
                let entity = chunk
                    .entity_path()
                    .to_string()
                    .trim_start_matches('/')
                    .to_owned();
                if entity.starts_with("topics/") {
                    assert_eq!(
                        store_id.kind(),
                        StoreKind::Recording,
                        "topic 数据不能写到 Blueprint store：{entity}"
                    );
                    assert!(
                        recording_ids.contains(&store_id),
                        "topic 数据须匹配 Recording 身份：{entity}"
                    );
                }
                recording.entities.insert(entity.clone());
                for descriptor in chunk.component_descriptors() {
                    if let Some(component_type) = descriptor.component_type {
                        let name = component_type.to_string();
                        recording
                            .components
                            .entry(entity.clone())
                            .or_default()
                            .insert(name.clone());
                        if name == "rerun.components.Text" {
                            for row in chunk
                                .iter_component::<rerun::components::Text>(descriptor.component)
                            {
                                for text in row.as_slice() {
                                    recording
                                        .texts
                                        .entry(entity.clone())
                                        .or_default()
                                        .push(text.0.to_string());
                                }
                            }
                        } else if name == "rerun.components.Scalar" {
                            for row in chunk
                                .iter_component::<rerun::components::Scalar>(descriptor.component)
                            {
                                for scalar in row.as_slice() {
                                    recording
                                        .scalars
                                        .entry(entity.clone())
                                        .or_default()
                                        .push(scalar.0.0);
                                }
                            }
                        } else if name == "rerun.components.ImageBuffer" {
                            for row in chunk.iter_component::<rerun::components::ImageBuffer>(
                                descriptor.component,
                            ) {
                                for buffer in row.as_slice() {
                                    recording
                                        .images
                                        .entry(entity.clone())
                                        .or_default()
                                        .push(buffer.0.0.to_vec());
                                }
                            }
                        } else if name == "rerun.components.Position3D" {
                            for row in chunk.iter_component::<rerun::components::Position3D>(
                                descriptor.component,
                            ) {
                                for position in row.as_slice() {
                                    recording
                                        .positions
                                        .entry(entity.clone())
                                        .or_default()
                                        .push(position.0.0);
                                }
                            }
                        } else if name == "rerun.components.TensorData" {
                            for row in chunk.iter_component::<rerun::components::TensorData>(
                                descriptor.component,
                            ) {
                                for tensor in row.as_slice() {
                                    recording
                                        .tensors
                                        .entry(entity.clone())
                                        .or_default()
                                        .push(tensor.0.clone());
                                }
                            }
                        }
                    }
                }
            }
            LogMsg::BlueprintActivationCommand(_) => {}
        }
    }
    assert!(store_info > 0, "必须含真实 recording 身份");
    assert!(
        !recording.entities.is_empty(),
        "不能用空文件或只有文件头冒充数据接入"
    );
    recording
}

fn stat(report: &Value, schema: u32, field: &str) -> u64 {
    report["per_schema"][schema.to_string()][field]
        .as_u64()
        .unwrap_or(0)
}

fn wait_for_mirrors(clock: &impl RobotClock, sources: &[Box<dyn fixtures::FixtureSource>]) {
    let deadline = clock.now().0 + 5_000_000_000;
    for source in sources {
        let monitor = source.monitor();
        loop {
            let state = monitor.snapshot();
            assert!(
                !state.finished,
                "镜像提前退出 {}：{state:?}",
                source.topic()
            );
            if state.ready {
                break;
            }
            assert!(
                clock.now().0 < deadline,
                "镜像未在监督上限内就绪 {}：{state:?}",
                source.topic()
            );
            clock.sleep_for(DurationNs(5_000_000));
        }
    }
}

fn drop_and_verify_mirrors(sources: Vec<Box<dyn fixtures::FixtureSource>>) {
    let monitors: Vec<_> = sources
        .iter()
        .map(|source| (source.topic().to_owned(), source.monitor()))
        .collect();
    drop(sources);
    for (topic, monitor) in monitors {
        let state = monitor.snapshot();
        assert!(
            state.finished && state.reaped,
            "镜像必须真实 join {topic}：{state:?}"
        );
        assert_eq!(
            (
                state.pending_frames,
                state.pending_bytes,
                state.join_failures
            ),
            (0, 0, 0),
            "镜像资源未完整回收 {topic}：{state:?}"
        );
        assert_eq!(
            state.estimated_shm_bytes, 0,
            "镜像 SHM 预约须随 worker 回收：{topic}"
        );
    }
}

fn instance_entity(path: &str, robot_id: &str, topic: &str, suffix: &str) -> bool {
    path.starts_with(&format!("topics/{robot_id}/{topic}/instances/"))
        && path.ends_with(&format!("/{suffix}"))
}

#[test]
#[ignore = "显式真实共享内存与 Rerun 子进程软件集成；无硬件"]
#[allow(clippy::too_many_lines)] // 同一真实录制中集中验证完整十类消息及媒体内容。
fn all_pubsub_schemas_reach_rerun_recording_without_cross_robot_leakage() {
    let _case = CASE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let clock = SystemClock::new();
    let robot_id = unique_robot(&clock, "all");
    let evidence = Evidence::new(&robot_id);
    let node = NodeBuilder::new()
        .create::<Service>()
        .expect("隔离软件发布 Node");
    let mut sources = fixtures::all_sources(&node, &robot_id);
    let expected: Vec<_> = sources
        .iter()
        .map(|source| (source.schema(), source.topic().to_owned()))
        .collect();
    sources.push(fixtures::foreign_source(&node, &robot_id));
    sources.push(fixtures::second_estop_source(&node, &robot_id));
    wait_for_mirrors(&clock, &sources);
    let report = run_bridge(&clock, &evidence, &robot_id, &mut sources);
    drop_and_verify_mirrors(sources);
    let recording = decode_recording(&evidence.root.join("observation.rrd"));
    assert_eq!(expected.len(), 10);
    assert_eq!(
        report["publisher_instances_seen"], 11,
        "多个合法发布者交替不能反复计数：{report}"
    );
    for (schema, topic) in expected {
        assert!(
            stat(&report, schema, "logged") > 0,
            "schema {schema} 未实际接入：{report}"
        );
        let root = format!("topics{topic}/");
        assert!(
            recording
                .entities
                .iter()
                .any(|entity| entity.starts_with(&root)),
            "RRD 缺实体 {root}"
        );
    }
    assert_eq!(
        report["frames_rejected"].as_u64(),
        Some(0),
        "有效十类夹具不应被拒：{report}"
    );
    let topics = report["topics_seen"]
        .as_array()
        .expect("机器报告必须列观测 topic");
    assert!(!topics.iter().any(|topic| {
        topic
            .as_str()
            .is_some_and(|topic| topic.contains(&format!("{robot_id}_other")))
    }));
    assert!(
        !recording
            .entities
            .iter()
            .any(|entity| entity.contains(&format!("{robot_id}_other")))
    );
    for (joint, value) in [("joint_a", 0.125), ("joint_b", -0.25)] {
        let suffix = format!("position_rad/{joint}");
        assert!(
            recording
                .scalars
                .iter()
                .any(
                    |(path, values)| instance_entity(path, &robot_id, "s0/arm/state", &suffix)
                        && values.contains(&value)
                ),
            "RRD 未保存真实关节值 {suffix}={value}"
        );
    }
    assert!(
        recording.images.iter().any(|(path, images)| {
            instance_entity(path, &robot_id, "sensors/camera/color", "image")
                && images.contains(&vec![255, 0, 0, 0, 255, 0])
        }),
        "图像组件必须保存原始像素"
    );
    assert!(
        recording.positions.iter().any(|(path, points)| {
            instance_entity(path, &robot_id, "sensors/lidar/points", "points")
                && points.contains(&[1.0, 2.0, 3.0])
                && points.contains(&[-1.0, -2.0, -3.0])
        }),
        "点云必须保存两个真实坐标"
    );
    assert!(recording.tensors.iter().any(|(path, tensors)| {
        instance_entity(path, &robot_id, "policy/test/observation", "tensor")
            && tensors.iter().any(|tensor| tensor.shape.as_ref() == [2, 2]
                && matches!(&tensor.buffer, rerun::encodings::TensorBuffer::F64(values) if values.as_ref() == [0.5, 1.5, 2.5, 3.5]))
    }), "张量必须保存形状与真实浮点数据");
    let event_entities: Vec<_> = recording
        .texts
        .iter()
        .filter(|(path, _)| instance_entity(path, &robot_id, "events/estop", "status"))
        .collect();
    assert_eq!(event_entities.len(), 2, "同 topic 的两个发布者必须分开保存");
    for reason in fixtures::ESTOP_REASONS {
        assert_eq!(
            event_entities
                .iter()
                .filter(|(_, texts)| texts.iter().any(|text| text.contains(reason)))
                .count(),
            1,
            "每个实例只能拥有本身事件内容：{reason}"
        );
    }
}

#[test]
#[ignore = "显式坏包与错误布局拒绝软件集成；无硬件"]
fn malformed_samples_and_wrong_layout_never_become_valid_visual_state() {
    let _case = CASE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let clock = SystemClock::new();
    let robot_id = unique_robot(&clock, "reject");
    let evidence = Evidence::new(&robot_id);
    let node = NodeBuilder::new()
        .create::<Service>()
        .expect("隔离软件发布 Node");
    let mut sources = fixtures::all_sources(&node, &robot_id);
    sources.retain(|source| matches!(source.schema(), SCHEMA_JOINT_LAYOUT | SCHEMA_JOINT_STATE));
    sources.extend(fixtures::invalid_sources(&node, &robot_id));
    wait_for_mirrors(&clock, &sources);
    let report = run_bridge(&clock, &evidence, &robot_id, &mut sources);
    drop_and_verify_mirrors(sources);
    assert!(
        stat(&report, SCHEMA_JOINT_STATE, "rejected") > 0,
        "错误布局/坏 FlatBuffer 未被拒绝：{report}"
    );
    assert!(
        stat(&report, SCHEMA_IMAGE_HEADER, "rejected") > 0,
        "错误图像尺寸未被拒绝：{report}"
    );
    assert!(
        stat(&report, SCHEMA_JOINT_STATE, "logged") > 0,
        "错误流不能阻止有效状态接入：{report}"
    );
    assert!(
        report["reject_reasons"]
            .as_array()
            .is_some_and(|reasons| reasons.len() >= 3),
        "应独立记录三类拒绝原因：{report}"
    );
    let recording = decode_recording(&evidence.root.join("observation.rrd"));
    assert!(
        !recording
            .entities
            .iter()
            .any(|entity| entity.starts_with(&format!(
                "topics/{robot_id}/s0/invalid_arm/state/instances/"
            )) && entity.contains("/position_rad/"))
    );
    assert!(
        !recording
            .scalars
            .values()
            .flatten()
            .any(|value| *value == fixtures::INVALID_POSITION)
    );
    assert!(!recording.entities.iter().any(|entity| instance_entity(
        entity,
        &robot_id,
        "sensors/bad_camera/color",
        "image"
    )));
}

fn launch_lifecycle(evidence: &Evidence, robot_id: &str, output: &[OsString]) -> ChildGuard {
    let config = evidence.root.join("robot.toml");
    fs::write(&config, fixtures::config(robot_id)).expect("写独立生命周期测试配置");
    let child = Command::new(env!("CARGO_BIN_EXE_robot-viz"))
        .arg("--config")
        .arg(config)
        .arg("--report")
        .arg(evidence.root.join("report.json"))
        .args(["--duration-ms", "60000"])
        .args(output)
        .env_remove("ROBOT_VIZ_INTERNAL_WORKER")
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(File::create(evidence.root.join("bridge.stdout.log")).unwrap())
        .stderr(File::create(evidence.root.join("bridge.stderr.log")).unwrap())
        .spawn()
        .expect("启动生命周期测试桥");
    ChildGuard(Some(child))
}

fn lifecycle_result(
    clock: &impl RobotClock,
    evidence: &Evidence,
    child: &mut ChildGuard,
) -> (ExitStatus, Value) {
    let deadline = clock.now().0 + 35_000_000_000;
    while child.child().try_wait().unwrap().is_none() {
        assert!(
            clock.now().0 < deadline,
            "生命周期监督未收口，证据在 {}",
            evidence.root.display()
        );
        clock.sleep_for(DurationNs(20_000_000));
    }
    let status = child.wait();
    let report: Value = serde_json::from_slice(
        &fs::read(evidence.root.join("report.json")).expect("必须保存真实退出报告"),
    )
    .unwrap();
    assert_eq!(
        report["worker_reaped"], true,
        "父进程必须真实回收 SDK worker：{report}"
    );
    eprintln!("生命周期软件证据：{}", evidence.root.display());
    (status, report)
}

#[test]
#[ignore = "显式真实输出初始化失败与 worker 回收；无硬件"]
fn invalid_output_directory_fails_and_reaps_worker() {
    let _case = CASE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let clock = SystemClock::new();
    let robot_id = unique_robot(&clock, "output_error");
    let evidence = Evidence::new(&robot_id);
    let output = evidence.root.join("absent_parent").join("observation.rrd");
    let mut child = launch_lifecycle(
        &evidence,
        &robot_id,
        &["--save".into(), output.into_os_string()],
    );
    let (status, report) = lifecycle_result(&clock, &evidence, &mut child);
    assert!(
        !status.success(),
        "输出初始化失败不能被报告为成功：{report}"
    );
    assert_eq!(report["output_status"], "error");
    assert!(
        report["error"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
            || report["supervisor_error"]
                .as_str()
                .is_some_and(|value| !value.is_empty())
    );
}

#[test]
#[ignore = "显式不可响应 viewer 的有界失败和真实回收；只使用测试自有 loopback socket"]
fn unresponsive_viewer_fails_without_leaving_sdk_worker() {
    let _case = CASE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let clock = SystemClock::new();
    let robot_id = unique_robot(&clock, "viewer_stalled");
    let evidence = Evidence::new(&robot_id);
    // 持有独占随机端口但不处理 gRPC；不依赖某固定端口恰好没有真实 viewer。
    let listener = TcpListener::bind("127.0.0.1:0").expect("绑定测试自有端口");
    let url = format!("rerun+http://{}/proxy", listener.local_addr().unwrap());
    let mut child = launch_lifecycle(&evidence, &robot_id, &["--connect".into(), url.into()]);
    let (status, report) = lifecycle_result(&clock, &evidence, &mut child);
    assert!(!status.success(), "viewer 不响应不能静默成功：{report}");
    assert_eq!(report["output_status"], "error");
    drop(listener);
}

#[test]
#[ignore = "显式 SIGINT 父进程到 SDK worker 的真实停止回收；无硬件"]
fn sigint_to_parent_stops_and_reaps_active_worker() {
    let _case = CASE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let clock = SystemClock::new();
    let robot_id = unique_robot(&clock, "sigint");
    let evidence = Evidence::new(&robot_id);
    let output = evidence.root.join("observation.rrd");
    let mut child = launch_lifecycle(
        &evidence,
        &robot_id,
        &["--save".into(), output.clone().into_os_string()],
    );
    let deadline = clock.now().0 + 15_000_000_000;
    while !fs::metadata(&output).is_ok_and(|metadata| metadata.len() > 0) {
        assert!(
            child.child().try_wait().unwrap().is_none(),
            "信号发送前桥已提前退出"
        );
        assert!(clock.now().0 < deadline, "信号测试未观察到实际 RRD 写入");
        clock.sleep_for(DurationNs(20_000_000));
    }
    let pid = i32::try_from(child.child().id()).unwrap();
    // SAFETY：只向测试持有且未回收的桥父进程发信号，由它负责停止其 SDK worker。
    assert_eq!(unsafe { libc::kill(pid, libc::SIGINT) }, 0);
    let (status, report) = lifecycle_result(&clock, &evidence, &mut child);
    assert!(status.success(), "正常信号收尾应保存成功报告：{report}");
    assert_eq!(report["output_status"], "success");
    decode_recording(&output);
}

#[test]
#[ignore = "显式 72 个累计短命 Action topic 跨真实闲置 TTL 回收再发现；无硬件"]
fn retired_action_topics_release_capacity_for_later_actions() {
    let _case = CASE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let clock = SystemClock::new();
    let robot_id = unique_robot(&clock, "topic_retirement");
    let evidence = Evidence::new(&robot_id);
    let node = NodeBuilder::new()
        .create::<Service>()
        .expect("独立事件测试 Node");
    let output = evidence.root.join("observation.rrd");
    let mut child = launch_lifecycle(
        &evidence,
        &robot_id,
        &[
            "--save".into(),
            output.clone().into_os_string(),
            "--max-frames".into(),
            "10000".into(),
        ],
    );
    let mut expected_topics = Vec::new();
    // 同时仅8个publisher，累计72个独立topic；最后一批须等待真实5秒闲置回收。
    for batch in 0..9 {
        let mut sources: Vec<_> = (0..8)
            .map(|offset| fixtures::action_source(&node, &robot_id, batch * 8 + offset))
            .collect();
        expected_topics.extend(sources.iter().map(|source| source.topic().to_owned()));
        wait_for_mirrors(&clock, &sources);
        let deadline = clock.now().0 + 12_000_000_000;
        loop {
            assert!(
                child.child().try_wait().unwrap().is_none(),
                "动态发现期间桥提前退出"
            );
            assert!(
                clock.now().0 < deadline,
                "第{batch}批未能发现；累计topic容量可能未释放"
            );
            for source in &mut sources {
                source.publish(clock.now().0);
            }
            if sources
                .iter()
                .all(|source| fixtures::mirror_has_reader(&node, source.topic()))
            {
                break;
            }
            clock.sleep_for(DurationNs(20_000_000));
        }
        // 已证明桥附着，再提供短接收窗口；最终断言仍以RRD真实数据为准。
        let hold_until = clock.now().0 + 200_000_000;
        while clock.now().0 < hold_until {
            for source in &mut sources {
                source.publish(clock.now().0);
            }
            clock.sleep_for(DurationNs(20_000_000));
        }
        drop_and_verify_mirrors(sources);
    }
    assert!(
        !fixtures::mirror_has_reader(&node, &expected_topics[0]),
        "桥仍运行时必须已经退订最早的闲置事件topic"
    );
    let pid = i32::try_from(child.child().id()).unwrap();
    // SAFETY：仅向本测试持有的桥父进程发送停止信号。
    assert_eq!(unsafe { libc::kill(pid, libc::SIGINT) }, 0);
    let (status, report) = lifecycle_result(&clock, &evidence, &mut child);
    assert!(status.success(), "短命topic回收后收尾失败：{report}");
    assert!(
        report["topics_discovered_total"]
            .as_u64()
            .is_some_and(|value| value >= 72),
        "累计发现必须超过64：{report}"
    );
    assert!(
        report["topics_retired"]
            .as_u64()
            .is_some_and(|value| value > 0),
        "必须有真实订阅回收：{report}"
    );
    assert!(
        report["topics_seen"]
            .as_array()
            .is_some_and(|topics| topics.len() <= 64),
        "报告历史必须有界：{report}"
    );
    let recording = decode_recording(&output);
    for topic in expected_topics {
        assert!(
            recording.entities.iter().any(|path| path
                .starts_with(&format!("topics{topic}/instances/"))
                && path.ends_with("/status")),
            "RRD 缺少累计事件topic {topic}"
        );
    }
}
