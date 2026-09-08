//! H2：真实 S0 的两种 `EStop` 入口、事务执行权内载荷与完整物理确认时序。

use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use iceoryx2::prelude::NodeBuilder;
use iceoryx2::service::ipc::Service;
use robot_core::time::{RobotClock, SystemClock};
use robot_msg::{build_helpers::build_estop_event, markers};
use robot_rpc::proto::robot_control_client::RobotControlClient;
use robot_rpc::proto::{ClearEStopCommand, GetStateRequest, SetEnabledCommand};
use robot_s0::comm_fault_hook::{HookCommandKind, TestCommFaultCommand};
use robot_s0::comm_fault_trace::{TRACE_CAPACITY, TraceEvent, TraceKind, TracePage};
use robot_s0::deploy::identity::load_and_verify_arm_config;
use robot_s0::deploy::types::DeployArm;
use robot_s0::rx_trust::{RealMarkerFs, acquire_trust_lock, marker_status};
use robot_transport::iceoryx2::{Iceoryx2Publisher, TopicClass, publisher};
use robot_transport::traits::Publisher;
use serde_json::{Value, json};
use tonic::transport::Channel;

use super::control::{ControlPort, wait_applied};
use super::harness::{Case, isolated_command, stop, wait_log};
use super::heartbeat::HeartbeatFeed;
use super::rpc::{command_meta, connect, gateway_command};
use super::{Args, Phase};

const WARMUP: usize = 100;
const SAMPLES: usize = 1000;
const MAX_ATTEMPTS: usize = (WARMUP + SAMPLES) * 10;
static COMMAND_SEQ: AtomicU64 = AtomicU64::new(10_000);

#[derive(Clone, Copy, Debug)]
struct LeaseInterval {
    sample_seq: u64,
    normal_seq: u64,
    generation: u32,
    acquired_ns: u64,
    released_ns: u64,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Sample {
    pub(super) t0_ns: u64,
    pub(super) t1_ns: u64,
    pub(super) all_off_ns: u64,
    pub(super) in_flight: bool,
    pub(super) primitive_retries: u64,
    in_flight_lease: Option<LeaseInterval>,
}

/// 只解释真实事件，不重建执行器状态机，也不根据时延挑选或删除样本。
pub(super) fn analyze(events: &[TraceEvent], external: bool) -> Result<Sample, String> {
    let epoch = events.first().ok_or("事件为空")?.sample_seq;
    if epoch == 0 || events.iter().any(|event| event.sample_seq != epoch) {
        return Err("同一样本混入了其它 epoch".to_owned());
    }
    let unique = |kind: TraceKind, successful_only: bool| -> Result<&TraceEvent, String> {
        let mut found = events
            .iter()
            .filter(|event| event.kind == kind && (!successful_only || event.b == 1));
        let event = found.next().ok_or_else(|| format!("缺少 {kind:?}"))?;
        if found.next().is_some() {
            return Err(format!("{kind:?} 不止一次"));
        }
        Ok(event)
    };
    let start = unique(
        if external {
            TraceKind::ExternalEnter
        } else {
            TraceKind::TopicReceived
        },
        false,
    )?;
    let primitive = unique(TraceKind::PrimitiveCommitted, false)?;
    if primitive.b > 1 {
        return Err("Merged/LatchOnly 不能充当一轮物理断扭".to_owned());
    }
    let serial_claim = unique(TraceKind::SerialClaim, true)?;
    let ipc_claim = unique(TraceKind::IpcClaim, true)?;
    let committed = unique(TraceKind::IpcEStopCommit, false)?;
    let all_off = unique(TraceKind::AllOffTrusted, false)?;
    validate_event_membership(events, external, primitive.generation)?;
    let safety = validate_physical_calls(events, all_off.stamp_ns)?;
    if !(start.stamp_ns <= primitive.stamp_ns
        && primitive.stamp_ns <= serial_claim.stamp_ns
        && primitive.stamp_ns <= ipc_claim.stamp_ns
        && serial_claim.stamp_ns <= safety
        && ipc_claim.stamp_ns <= committed.stamp_ns)
    {
        return Err("采样因果链顺序不成立".to_owned());
    }
    let in_flight_lease =
        classify_interval(events, start.stamp_ns, primitive, safety, all_off.stamp_ns)?;
    Ok(Sample {
        t0_ns: start.stamp_ns,
        t1_ns: primitive.stamp_ns,
        all_off_ns: all_off.stamp_ns,
        in_flight: in_flight_lease.is_some(),
        primitive_retries: primitive.a,
        in_flight_lease,
    })
}

fn validate_event_membership(
    events: &[TraceEvent],
    external: bool,
    generation: u32,
) -> Result<(), String> {
    for event in events {
        match event.kind {
            TraceKind::TopicReceived if external => {
                return Err("同一 epoch 混入 Topic 入口".to_owned());
            }
            TraceKind::ExternalEnter if !external => {
                return Err("同一 epoch 混入 External 入口".to_owned());
            }
            TraceKind::CommErrorObserved
            | TraceKind::FirstCommError
            | TraceKind::MailboxPublishing
            | TraceKind::MailboxPublished
            | TraceKind::FaultIpcCommit => return Err("健康 SLA 样本含真实故障事件".to_owned()),
            TraceKind::TopicReceived | TraceKind::ExternalEnter => {}
            _ if event.generation != generation => return Err("样本拼接了不同故障代次".to_owned()),
            _ => {}
        }
        if matches!(
            event.kind,
            TraceKind::NormalAcquire
                | TraceKind::NormalRelease
                | TraceKind::SerialClaim
                | TraceKind::IpcClaim
                | TraceKind::SafetyReturn
                | TraceKind::ConfirmReturn
        ) && event.b > 1
        {
            return Err("事件的成功标志不是 bool".to_owned());
        }
    }
    Ok(())
}

fn validate_physical_calls(events: &[TraceEvent], all_off: u64) -> Result<u64, String> {
    let mut active_safety = None;
    let mut first_start = None;
    let mut attempts = 0;
    let mut delivered = None;
    let mut last_confirm = None;
    let mut confirmations = 0;
    for event in events {
        match event.kind {
            TraceKind::SafetyStart => {
                if active_safety.is_some()
                    || delivered.is_some()
                    || event.a != attempts + 1
                    || event.a > 3
                {
                    return Err("Safety attempt 重复、跳号或成功后额外调用".to_owned());
                }
                first_start.get_or_insert(event.stamp_ns);
                active_safety = Some(event);
                attempts += 1;
            }
            TraceKind::SafetyReturn => {
                let started: &TraceEvent =
                    active_safety.take().ok_or("Safety return 没有 start")?;
                if started.a != event.a || started.stamp_ns > event.stamp_ns {
                    return Err("Safety start/return attempt 或时间不匹配".to_owned());
                }
                if event.b == 1 {
                    delivered = Some(event.stamp_ns);
                }
            }
            TraceKind::ConfirmReturn => {
                let sent = delivered.ok_or("Confirm 之前没有成功 Safety")?;
                if event.stamp_ns < sent
                    || last_confirm.is_some_and(|previous| event.stamp_ns <= previous)
                    || event.stamp_ns > all_off
                    || event.a > 32
                    || confirmations >= 5
                {
                    return Err("Confirm 的调用顺序、数量或时刻无效".to_owned());
                }
                if (event.a == 0 && event.b == 1) != (event.stamp_ns == all_off) {
                    return Err("全关边界必须精确对应唯一的成功全关 Confirm".to_owned());
                }
                confirmations += 1;
                last_confirm = Some(event.stamp_ns);
            }
            _ => {}
        }
    }
    if active_safety.is_some() || delivered.is_none() || last_confirm != Some(all_off) {
        return Err("物理调用没有完整成对退场及唯一全关证据".to_owned());
    }
    first_start.ok_or_else(|| "没有真实 Safety 调用".to_owned())
}

fn classify_interval(
    events: &[TraceEvent],
    t0: u64,
    primitive: &TraceEvent,
    safety: u64,
    end: u64,
) -> Result<Option<LeaseInterval>, String> {
    let mut acquired = None;
    let mut in_flight = None;
    let mut active_at_commit = false;
    for event in events.iter().filter(|event| {
        event.stamp_ns < end
            && matches!(
                event.kind,
                TraceKind::NormalAcquire | TraceKind::NormalRelease
            )
    }) {
        match event.kind {
            TraceKind::NormalAcquire if event.b == 1 => {
                if event.stamp_ns >= primitive.stamp_ns {
                    return Err("原语提交后、物理收敛前启动了新 normal 事务".to_owned());
                }
                if acquired.replace(event).is_some() {
                    return Err("normal 执行权重复 acquire".to_owned());
                }
            }
            TraceKind::NormalRelease if event.b == 1 => {
                if event.stamp_ns > safety {
                    return Err("同总线 Safety 开始时 normal 尚未释放".to_owned());
                }
                if let Some(begin) = acquired.take() {
                    if begin.normal_seq != event.normal_seq {
                        return Err("normal 执行权序号不匹配".to_owned());
                    }
                    if begin.stamp_ns <= t0 && t0 < event.stamp_ns {
                        in_flight = Some(LeaseInterval {
                            sample_seq: begin.sample_seq,
                            normal_seq: begin.normal_seq,
                            generation: begin.generation,
                            acquired_ns: begin.stamp_ns,
                            released_ns: event.stamp_ns,
                        });
                    }
                    active_at_commit |=
                        begin.stamp_ns <= primitive.stamp_ns && primitive.stamp_ns < event.stamp_ns;
                } else if event.stamp_ns > t0 {
                    return Err("在途执行权缺少同 epoch 的 acquire，不能归为 idle".to_owned());
                }
            }
            _ => {}
        }
    }
    // old ACTIVE 来自真实成功 CAS；CAS 之后重读的 normal_seq 可能已经被 serial 清零。
    if acquired.is_some() || (primitive.b & 1 != 0) != active_at_commit {
        return Err("原语的旧 ACTIVE 与完整 acquire/release 区间不一致".to_owned());
    }
    Ok(in_flight)
}

/// 仅用于 H2 在途组：真实随机附加载荷必须完整落在 t0 所属事务的执行权内。
pub(super) fn validate_inflight_delay(
    events: &[TraceEvent],
    sample: &Sample,
) -> Result<(), String> {
    let lease = sample
        .in_flight_lease
        .ok_or("在途载荷没有对应的真实执行权区间")?;
    let mut observed = None;
    for event in events
        .iter()
        .filter(|event| event.kind == TraceKind::InjectedDelay)
    {
        // 生产 stamp 是 sleep 开始，a 是请求时长，b 是 sleep 返回后实测耗时。
        let finished = event
            .stamp_ns
            .checked_add(event.b)
            .ok_or("随机载荷时间溢出")?;
        let starts_inside = (lease.acquired_ns..=lease.released_ns).contains(&event.stamp_ns);
        let overlaps = event.stamp_ns < lease.released_ns && finished > lease.acquired_ns;
        if !starts_inside && !overlaps {
            continue;
        }
        if observed.replace(event).is_some() {
            return Err("同一事务出现重复或重叠的随机载荷记录".to_owned());
        }
        if event.sample_seq != lease.sample_seq
            || event.generation != lease.generation
            || event.normal_seq != lease.normal_seq
        {
            return Err("随机载荷与实际执行权的 epoch/gen/normal_seq 不匹配".to_owned());
        }
        if !(1_000_000..=5_000_000).contains(&event.a) || event.b < event.a {
            return Err("随机载荷不满足请求1–5ms且实际耗时不小于请求".to_owned());
        }
        if event.stamp_ns < lease.acquired_ns || finished > lease.released_ns {
            return Err("随机载荷的完整时间区间越出对应执行权".to_owned());
        }
    }
    observed
        .ok_or_else(|| "在途样本缺少真实 InjectedDelay 证据".to_owned())
        .map(|_| ())
}

pub(super) fn distribution(values: &[u64]) -> Value {
    if values.is_empty() {
        return Value::Null;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let percentile =
        |fraction: usize| sorted[(sorted.len() * fraction).div_ceil(100).saturating_sub(1)];
    json!({"count": values.len(), "p50": percentile(50), "p99": percentile(99),
        "max": sorted[sorted.len()-1]})
}

fn sha256(path: &Path) -> Result<String, String> {
    let output = std::process::Command::new("sha256sum")
        .arg(path)
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!("SHA256 失败：{}", path.display()));
    }
    let text = String::from_utf8(output.stdout).map_err(|e| e.to_string())?;
    let hash = text.split_whitespace().next().ok_or("SHA256 输出为空")?;
    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("非法 SHA256".to_owned());
    }
    Ok(hash.to_owned())
}

fn verify_artifact(directory: &Path, name: &str, required: &[&str]) -> Result<Value, String> {
    let path = directory.join(name);
    let hash = sha256(&path)?;
    let bytes = std::fs::read(directory.join(format!("{name}.build.json")))
        .map_err(|e| format!("缺少真实构建记录：{e}"))?;
    let manifest: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if manifest["sha256"].as_str() != Some(hash.as_str()) {
        return Err(format!("{name} 构建记录 SHA 不符"));
    }
    let features = manifest["features"]
        .as_array()
        .ok_or("构建 features 必须为明确数组")?;
    for feature in required {
        if !features.iter().any(|value| value.as_str() == Some(feature)) {
            return Err(format!("{name} 未声明必要 feature {feature}"));
        }
    }
    Ok(manifest)
}

fn physical_case(args: &Args) -> Result<(Case, String, Value), String> {
    if args.phase != Phase::OrinSla || !cfg!(feature = "real-hw") {
        return Err("H2 仅允许显式 orin-sla 与 real-hw 组合".to_owned());
    }
    let arm = match args.hardware_id.as_deref() {
        Some("left_arm") => DeployArm::Left,
        Some("right_arm") => DeployArm::Right,
        _ => return Err("H2 必须选择 D2 的 left_arm/right_arm".to_owned()),
    };
    let config = args.config.as_ref().ok_or("缺少现场配置")?;
    let verified = load_and_verify_arm_config(arm, config).map_err(|e| e.to_string())?;
    let current_exe = std::fs::canonicalize(std::env::current_exe().map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    if current_exe
        != std::fs::canonicalize(args.bin_dir.join("comm_fault_e2e")).map_err(|e| e.to_string())?
    {
        return Err("current_exe 与 --bin-dir 中的验收工具不一致".to_owned());
    }
    let artifacts = json!({
        "tool": verify_artifact(&args.bin_dir, "comm_fault_e2e", &["comm-fault-e2e", "real-hw"] )?,
        "s0": verify_artifact(&args.bin_dir, "s0_daemon", &["test-comm-fault-hook", "xlerobot-real"] )?,
        "gateway": verify_artifact(&args.bin_dir, "robot_rpc_gateway", &[])?,
    });
    let model = std::fs::read("/proc/device-tree/model").map_err(|e| e.to_string())?;
    let model = String::from_utf8_lossy(&model)
        .trim_end_matches('\0')
        .to_owned();
    if !model.to_ascii_lowercase().contains("orin") {
        return Err(format!("非 Orin 判据机器：{model}"));
    }
    let udev = std::process::Command::new("udevadm")
        .args(["info", "--query=property", "--name"])
        .arg(&verified.address)
        .output()
        .map_err(|e| e.to_string())?;
    if !udev.status.success() {
        return Err("udevadm 身份读取失败".to_owned());
    }
    let properties = String::from_utf8(udev.stdout).map_err(|e| e.to_string())?;
    for expected in [
        "ID_VENDOR_ID=1a86",
        "ID_MODEL_ID=55d3",
        "ID_USB_DRIVER=cdc_acm",
    ] {
        if !properties.lines().any(|line| line == expected) {
            return Err(format!("udev 不满足 {expected}"));
        }
    }
    let directory = args.evidence_dir.join(format!("orin_sla_{}", arm.label()));
    std::fs::create_dir(&directory).map_err(|e| format!("证据目录必须新建：{e}"))?;
    let trust = std::path::PathBuf::from("/var/lib/robot").join(&verified.robot_id);
    // 与正式 S0 使用同一个实例锁。只验证已由现场明确授予的一次性许可，不代写授权。
    {
        let _lock = acquire_trust_lock(&trust).map_err(|e| format!("实例锁拒绝：{e:?}"))?;
        if !marker_status(&RealMarkerFs, &trust)
            .map_err(|e| e.to_string())?
            .authorized_candidate()
        {
            return Err("H2 前必须用真实 rx-trust authorize 显式授权本次启动".to_owned());
        }
    }
    let environment = json!({"model": model, "udev": properties, "artifacts": artifacts,
        "current_exe": current_exe, "config_sha256": sha256(config)?,
        "calibration_sha256": sha256(Path::new(&verified.calibration_path))?,
        "address": verified.address, "hardware_id": verified.hardware_id,
        "kernel": std::fs::read_to_string("/proc/version").map_err(|e| e.to_string())?,
        "governor": std::fs::read_to_string("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor").ok(),
        "l4t": std::fs::read_to_string("/etc/nv_tegra_release").ok()});
    let case = Case {
        directory,
        config: config.clone(),
        robot_id: verified.robot_id,
        latch: trust.join("estop_latch"),
        trust,
        bin_dir: args.bin_dir.clone(),
    };
    Ok((case, verified.hardware_id, environment))
}

async fn send_control(
    port: &ControlPort,
    case: &Case,
    s0: &mut tokio::process::Child,
    kind: HookCommandKind,
    target: u8,
    duration_ms: u32,
    count: u32,
) -> Result<u64, String> {
    let sequence = COMMAND_SEQ.fetch_add(1, Ordering::Relaxed);
    port.send(TestCommFaultCommand {
        sequence,
        duration_ms,
        count,
        kind: kind as u8,
        target,
        error: 0,
        reserved: [0; 5],
    })?;
    wait_applied(case, s0, sequence).await?;
    Ok(sequence)
}

fn publish_estop(
    output: &mut Iceoryx2Publisher<markers::EStopEvent>,
    sequence: u64,
) -> Result<(), String> {
    let now = SystemClock.now().0;
    let body = build_estop_event("H2受监督时序测量", now, "comm_fault_e2e", true);
    let mut loan = output.loan(1024 + body.len()).map_err(|e| e.to_string())?;
    loan.set_seq(sequence);
    loan.set_source_id(42);
    loan.set_stamp_ns(now);
    loan.set_body_bytes(&body);
    loan.publish().map_err(|e| e.to_string())
}

async fn recover(
    client: &mut RobotControlClient<Channel>,
    hardware_id: &str,
) -> Result<(), String> {
    // 让上一轮的真实确认与 L0 连续窗口完成；不通过观测页写入许可。
    tokio::time::sleep(Duration::from_millis(600)).await;
    let mut meta = command_meta(false, 2000);
    "maintainer-fake-token-001".clone_into(&mut meta.auth.as_mut().expect("构造器含 auth").token);
    let cleared = client
        .clear_e_stop(ClearEStopCommand {
            meta: Some(meta),
            confirmation: "H2-supervised-sample".to_owned(),
        })
        .await
        .map_err(|e| format!("真实 ClearEStop 失败：{e}"))?
        .into_inner();
    let enabled = client
        .set_enabled(SetEnabledCommand {
            meta: Some(command_meta(false, 2000)),
            enabled: true,
        })
        .await
        .map_err(|e| format!("真实 SetEnabled 失败：{e}"))?
        .into_inner();
    if cleared.dry_run
        || enabled.dry_run
        || cleared.cleared_at_ns == 0
        || enabled.cleared_at_ns == 0
    {
        return Err("恢复必须取得两项真实完成回执".to_owned());
    }
    let deadline = SystemClock.now().0 + 1_000_000_000;
    loop {
        let state = client
            .get_state(GetStateRequest {
                hardware_id: hardware_id.to_owned(),
            })
            .await
            .map_err(|e| e.to_string())?
            .into_inner();
        if state.mode == 3
            && state.control_link == 1
            && state.output_mode == 1
            && state.safety_state == 1
            && state.fault_code == 0
        {
            return Ok(());
        }
        if SystemClock.now().0 >= deadline {
            return Err(format!(
                "真实恢复后未观测 Enabled/Active/TorqueOn：{state:?}"
            ));
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

struct Background {
    stop: Arc<AtomicBool>,
    rr: tokio::task::JoinHandle<Vec<Value>>,
    scheduler: std::thread::JoinHandle<Vec<u64>>,
}

impl Background {
    fn start(mut client: RobotControlClient<Channel>) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let scheduler_stop = stop.clone();
        let scheduler = std::thread::Builder::new()
            .name("H2调度延迟探针".to_owned())
            .spawn(move || {
                let mut delays = Vec::new();
                let mut due = SystemClock.now().0 + 5_000_000;
                while !scheduler_stop.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_nanos(
                        due.saturating_sub(SystemClock.now().0),
                    ));
                    delays.push(SystemClock.now().0.saturating_sub(due));
                    due = due.saturating_add(5_000_000);
                }
                delays
            })
            .map_err(|e| e.to_string())?;
        let rr_stop = stop.clone();
        let rr = tokio::spawn(async move {
            let mut requests = Vec::new();
            let mut due = SystemClock.now().0;
            while !rr_stop.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_nanos(
                    due.saturating_sub(SystemClock.now().0),
                ))
                .await;
                if rr_stop.load(Ordering::Acquire) {
                    break;
                }
                let started = SystemClock.now().0;
                // 干跑请求仍经过真实 RR 调度与 evaluator，永不写电机。
                let response = client
                    .set_enabled(SetEnabledCommand {
                        meta: Some(command_meta(true, 2000)),
                        enabled: false,
                    })
                    .await;
                requests.push(json!({"due_ns": due, "started_ns": started,
                    "finished_ns": SystemClock.now().0, "ok": response.is_ok(),
                    "response": format!("{response:?}")}));
                due = due.saturating_add(50_000_000);
            }
            requests
        });
        Ok(Self {
            stop,
            rr,
            scheduler,
        })
    }

    async fn finish(self) -> Result<Value, String> {
        self.stop.store(true, Ordering::Release);
        let rr = self.rr.await.map_err(|e| e.to_string())?;
        let scheduler = self.scheduler.join().map_err(|_| "调度探针 panic")?;
        let starts: Vec<_> = rr
            .iter()
            .filter_map(|request| request["started_ns"].as_u64())
            .collect();
        let gaps: Vec<_> = starts
            .windows(2)
            .map(|pair| pair[1].saturating_sub(pair[0]))
            .collect();
        let valid = rr.len() >= 2 && rr.iter().all(|request| request["ok"] == true);
        let elapsed = starts
            .last()
            .copied()
            .unwrap_or(0)
            .saturating_sub(starts.first().copied().unwrap_or(0));
        // 不用四舍五入把不足的负载改写成 20/s；保留真实间隔和完整 response。
        let interval_count =
            u32::try_from(starts.len().saturating_sub(1)).map_err(|e| e.to_string())?;
        let actual_rate = if elapsed == 0 {
            0.0
        } else {
            f64::from(interval_count) / Duration::from_nanos(elapsed).as_secs_f64()
        };
        let cadence_valid = rr.iter().all(|request| {
            request["started_ns"]
                .as_u64()
                .unwrap_or(u64::MAX)
                .saturating_sub(request["due_ns"].as_u64().unwrap_or(0))
                < 50_000_000
        });
        Ok(json!({"valid_responses": valid, "rr_target_per_second": 20,
            "valid_cadence": cadence_valid,
            "rr_actual_per_second": actual_rate, "rr_requests": rr,
            "rr_gap_ns": distribution(&gaps), "sigma_probe_period_ns": 5_000_000,
            "sigma_est_ns": distribution(&scheduler), "sigma_raw_ns": scheduler}))
    }
}

fn save_events(
    csv: &mut std::fs::File,
    attempt: usize,
    external: bool,
    events: &[TraceEvent],
) -> Result<(), String> {
    for event in events {
        writeln!(
            csv,
            "{attempt},{external},{},{},{:?},{},{},{},{},{}",
            event.sample_seq,
            event.stamp_ns,
            event.kind,
            event.source,
            event.normal_seq,
            event.generation,
            event.a,
            event.b
        )
        .map_err(|e| e.to_string())?;
    }
    csv.flush().map_err(|e| e.to_string())
}

async fn collect(
    trace: &TracePage,
    sequence: u64,
    s0: &mut tokio::process::Child,
    events: &mut Vec<TraceEvent>,
) -> Result<(), String> {
    collect_batches(
        sequence,
        events,
        || trace.drain(TRACE_CAPACITY),
        || trace.dropped(),
        || {
            s0.try_wait()
                .map(|status| status.is_none())
                .map_err(|error| error.to_string())
        },
    )
    .await
}

/// 与真实采集器共用的循环；可替换的只有一次 drain 与外部存活事实。
pub(super) async fn collect_batches(
    sequence: u64,
    events: &mut Vec<TraceEvent>,
    mut drain: impl FnMut() -> Vec<TraceEvent>,
    dropped: impl Fn() -> u64,
    mut alive: impl FnMut() -> Result<bool, String>,
) -> Result<(), String> {
    let deadline = SystemClock.now().0 + 2_000_000_000;
    loop {
        // 已消费的原始事件必须全部交还调用方保存；不能先过滤外代再宣称完整。
        events.extend(drain());
        if events.iter().any(|event| event.sample_seq != sequence) {
            return Err("采集过程中混入其它 epoch，原始事件已保留".to_owned());
        }
        if dropped() != 0 {
            return Err("事件环累计 dropped>0，整次运行无效".to_owned());
        }
        if events
            .iter()
            .any(|event| event.kind == TraceKind::AllOffTrusted)
            && events
                .iter()
                .any(|event| event.kind == TraceKind::IpcEStopCommit)
            && events
                .iter()
                .any(|event| event.kind == TraceKind::SerialClaim && event.b == 1)
            && events
                .iter()
                .any(|event| event.kind == TraceKind::IpcClaim && event.b == 1)
        {
            events.sort_by_key(|event| event.stamp_ns);
            return Ok(());
        }
        if !alive()? {
            return Err("被测 S0 提前退出".to_owned());
        }
        if SystemClock.now().0 >= deadline {
            return Err(format!("没有完整物理完成事件：{events:?}"));
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

struct SlaRunner<'a> {
    case: &'a Case,
    port: &'a ControlPort,
    trace: &'a TracePage,
    s0: &'a mut tokio::process::Child,
    client: &'a mut RobotControlClient<Channel>,
    topic: &'a mut Iceoryx2Publisher<markers::EStopEvent>,
    csv: &'a mut std::fs::File,
    attempts: std::fs::File,
    hardware_id: &'a str,
}

impl SlaRunner<'_> {
    async fn software_samples(&mut self) -> Result<Vec<Value>, String> {
        let mut samples = Vec::new();
        for external in [false, true] {
            for attempt in 0..2 {
                let (sample, events) = self.attempt(attempt, external, false).await?;
                if sample.in_flight {
                    return Err("现有 mock 不提供真实总线 acquire，不能宣称在途采样".to_owned());
                }
                let mut safety_started = None;
                let mut safety_durations = Vec::new();
                for event in &events {
                    match event.kind {
                        TraceKind::SafetyStart => safety_started = Some(event.stamp_ns),
                        TraceKind::SafetyReturn => {
                            let started = safety_started.take().ok_or("Safety 返回无入口")?;
                            let elapsed = event
                                .stamp_ns
                                .checked_sub(started)
                                .ok_or("Safety 时间倒序")?;
                            safety_durations.push(elapsed);
                            if elapsed >= 10_000_000 {
                                return Err(format!(
                                    "AC13 mock 单次 Safety 调用不小于10ms：{elapsed}ns"
                                ));
                            }
                        }
                        _ => {}
                    }
                }
                let evidence = json!({"external": external, "cycle": attempt,
                    "epoch": events[0].sample_seq, "t0_ns": sample.t0_ns, "t1_ns": sample.t1_ns,
                    "all_off_ns": sample.all_off_ns, "real_rpc_recovery_verified": true,
                    "mock_safety_call_ns": safety_durations, "in_flight": false});
                self.record_attempt(&evidence)?;
                samples.push(evidence);
            }
        }
        Ok(samples)
    }

    async fn group(&mut self, external: bool, in_flight: bool) -> Result<Value, String> {
        send_control(
            self.port,
            self.case,
            self.s0,
            HookCommandKind::NormalDelayProfile,
            0,
            u32::from(in_flight),
            if in_flight { 5 } else { 0 },
        )
        .await?;
        let mut stats = GroupStats::default();
        for attempt in 0..MAX_ATTEMPTS {
            let (sample, events) = self.attempt(attempt, external, in_flight).await?;
            let matched = sample.in_flight == in_flight;
            self.record_attempt(
                &json!({"sequence": events[0].sample_seq, "attempt": attempt,
                "external": external, "requested_in_flight": in_flight,
                "matched": matched, "warmup": stats.completed < WARMUP,
                "t0_ns": sample.t0_ns, "t1_ns": sample.t1_ns, "all_off_ns": sample.all_off_ns}),
            )?;
            if matched {
                stats.accept(sample, &events);
            } else {
                stats.mismatches += 1;
            }
            if stats.completed == WARMUP + SAMPLES {
                break;
            }
        }
        let result = stats.evidence(external, in_flight);
        write_json(
            &self
                .case
                .directory
                .join(format!("sla_{external}_{in_flight}.json")),
            &result,
        )?;
        if !result["passed"].as_bool().unwrap_or(false) {
            return Err("H2 样本数或 P99 不满足判据，完整原始证据已保留".to_owned());
        }
        Ok(result)
    }

    fn record_attempt(&mut self, value: &Value) -> Result<(), String> {
        writeln!(self.attempts, "{value}").map_err(|e| e.to_string())?;
        self.attempts.flush().map_err(|e| e.to_string())
    }

    async fn attempt(
        &mut self,
        attempt: usize,
        external: bool,
        in_flight: bool,
    ) -> Result<(Sample, Vec<TraceEvent>), String> {
        recover(self.client, self.hardware_id).await?;
        let unarmed = self.trace.drain(TRACE_CAPACITY);
        // 上一 epoch 必须已完整消费并解除采样；迟到事件同样保留，不静默丢弃。
        save_events(self.csv, attempt, external, &unarmed)?;
        let sequence = send_control(
            self.port,
            self.case,
            self.s0,
            HookCommandKind::ArmSample,
            0,
            0,
            0,
        )
        .await?;
        let mut events = Vec::new();
        let captured = async {
            if in_flight {
                self.wait_acquired(sequence, &mut events).await?;
            }
            if external {
                send_control(
                    self.port,
                    self.case,
                    self.s0,
                    HookCommandKind::ExternalEStop,
                    0,
                    0,
                    0,
                )
                .await?;
            } else {
                publish_estop(self.topic, sequence)?;
            }
            collect(self.trace, sequence, self.s0, &mut events).await
        }
        .await;
        events.extend(self.trace.drain(TRACE_CAPACITY));
        events.sort_by_key(|event| event.stamp_ns);
        save_events(self.csv, attempt, external, &events)?;
        let disarmed = send_control(
            self.port,
            self.case,
            self.s0,
            HookCommandKind::ArmSample,
            1,
            0,
            0,
        )
        .await;
        let interpreted = captured
            .and(disarmed.map(|_| ()))
            .and_then(|()| analyze(&events, external))
            .and_then(|sample| {
                if in_flight && sample.in_flight {
                    validate_inflight_delay(&events, &sample)?;
                }
                Ok(sample)
            });
        match interpreted {
            Ok(sample) => Ok((sample, events)),
            Err(error) => {
                self.record_attempt(&json!({"sequence": sequence, "attempt": attempt,
                    "external": external, "requested_in_flight": in_flight,
                    "invalid": true, "error": error, "event_count": events.len(),
                    "dropped": self.trace.dropped()}))?;
                Err(error)
            }
        }
    }

    async fn wait_acquired(
        &mut self,
        sequence: u64,
        events: &mut Vec<TraceEvent>,
    ) -> Result<(), String> {
        let deadline = SystemClock.now().0 + 1_000_000_000;
        loop {
            events.extend(self.trace.drain(TRACE_CAPACITY));
            if self.trace.dropped() != 0 {
                return Err("事件环累计 dropped>0，整次运行无效".to_owned());
            }
            if events
                .iter()
                .rev()
                .find(|event| {
                    event.sample_seq == sequence
                        && matches!(
                            event.kind,
                            TraceKind::NormalAcquire | TraceKind::NormalRelease
                        )
                })
                .is_some_and(|event| event.kind == TraceKind::NormalAcquire && event.b == 1)
            {
                return Ok(());
            }
            if SystemClock.now().0 >= deadline {
                return Err("未观察到真实 normal acquire".to_owned());
            }
            tokio::time::sleep(Duration::from_micros(100)).await;
        }
    }
}

#[derive(Default)]
struct GroupStats {
    completed: usize,
    mismatches: usize,
    a_values: Vec<u64>,
    b_values: Vec<u64>,
    primitive_retries: Vec<u64>,
    acquire_retries: Vec<u64>,
    serial_claim_retries: Vec<u64>,
    ipc_claim_retries: Vec<u64>,
}

impl GroupStats {
    fn accept(&mut self, sample: Sample, events: &[TraceEvent]) {
        if self.completed >= WARMUP {
            self.a_values.push(sample.t1_ns - sample.t0_ns);
            self.b_values.push(sample.all_off_ns - sample.t0_ns);
            self.primitive_retries.push(sample.primitive_retries);
            for event in events {
                match event.kind {
                    TraceKind::NormalAcquire => self.acquire_retries.push(event.a),
                    TraceKind::SerialClaim => self.serial_claim_retries.push(event.a),
                    TraceKind::IpcClaim => self.ipc_claim_retries.push(event.a),
                    _ => {}
                }
            }
        }
        self.completed += 1;
    }

    fn evidence(&self, external: bool, in_flight: bool) -> Value {
        let a = distribution(&self.a_values);
        let b = distribution(&self.b_values);
        let b_target = if in_flight { 75_000_000 } else { 20_000_000 };
        let passed = self.completed == WARMUP + SAMPLES
            && a["p99"].as_u64().is_some_and(|value| value <= 2_000_000)
            && b["p99"].as_u64().is_some_and(|value| value <= b_target);
        json!({"external": external, "in_flight": in_flight, "warmup": WARMUP,
            "samples": self.a_values.len(), "class_mismatch_retries": self.mismatches,
            "sla_a_ns": a, "sla_b_ns": b, "passed": passed,
            "primitive_cas_retries": distribution(&self.primitive_retries),
            "acquire_cas_retries": distribution(&self.acquire_retries),
            "serial_claim_cas_retries": distribution(&self.serial_claim_retries),
            "ipc_claim_cas_retries": distribution(&self.ipc_claim_retries),
            "sla_b_target_ns": b_target,
            "fast_path_reference_ns": if in_flight { 265_000_000 } else { 160_000_000 },
            "full_path_first_probe_reference_ns": if in_flight { 375_000_000 } else { 270_000_000 },
            "full_path_five_probes_reference_ns": if in_flight { 795_000_000 } else { 690_000_000 },
            "scope": "参考界另加相应调度延迟；载荷=执行权内U[1,5]ms随机附加延迟+真实串口IO，非每W/R独立U分布"})
    }
}

fn write_json(path: &Path, value: &Value) -> Result<(), String> {
    std::fs::write(
        path,
        serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())
}

async fn measure(
    case: &Case,
    port: &ControlPort,
    s0: &mut tokio::process::Child,
    client: &mut RobotControlClient<Channel>,
    hardware_id: &str,
    physical: bool,
) -> Result<Value, String> {
    let trace = case.trace("s0")?;
    if !trace.installed() {
        return Err("S0 没有安装真实事件环".to_owned());
    }
    let heartbeat = HeartbeatFeed::start(&case.robot_id)?;
    let node = NodeBuilder::new()
        .create::<Service>()
        .map_err(|e| format!("{e:?}"))?;
    let mut topic = publisher::<markers::EStopEvent>(
        &node,
        &format!("/{}/events/estop", case.robot_id),
        TopicClass::SafetyEvent,
    )
    .map_err(|e| e.to_string())?;
    let mut csv =
        std::fs::File::create(case.directory.join("events.csv")).map_err(|e| e.to_string())?;
    writeln!(
        csv,
        "attempt,external,sample_seq,stamp_ns,kind,source,normal_seq,generation,a,b"
    )
    .map_err(|e| e.to_string())?;
    let attempts =
        std::fs::File::create(case.directory.join("attempts.jsonl")).map_err(|e| e.to_string())?;
    send_control(port, case, s0, HookCommandKind::ExternalEStop, 0, 0, 0).await?;
    let background = if physical {
        Some(Background::start(client.clone())?)
    } else {
        None
    };
    let mut runner = SlaRunner {
        case,
        port,
        trace: &trace,
        s0,
        client,
        topic: &mut topic,
        csv: &mut csv,
        attempts,
        hardware_id,
    };
    let mut groups = Vec::new();
    let measurement = async {
        if physical {
            for external in [false, true] {
                for in_flight in [false, true] {
                    heartbeat.verify_running()?;
                    groups.push(runner.group(external, in_flight).await?);
                }
            }
        } else {
            heartbeat.verify_running()?;
            groups = runner.software_samples().await?;
        }
        Ok::<(), String>(())
    }
    .await;
    let background = match background {
        Some(background) => background.finish().await?,
        None => Value::Null,
    };
    write_json(&case.directory.join("background.json"), &background)?;
    measurement?;
    if physical && (background["valid_responses"] != true || background["valid_cadence"] != true) {
        return Err("真实 RR 背景负载发生错误或未维持 20/s 调度，完整记录已保留".to_owned());
    }
    if trace.dropped() != 0 {
        return Err("累计事件丢失，整次 H2 运行无效".to_owned());
    }
    Ok(
        json!({"groups": groups, "background": background, "trace_dropped": trace.dropped(),
        "events_csv": case.directory.join("events.csv"), "hardware_only": physical, "runtime_only": !physical, "in_flight_exercised": physical,
        "scope": if physical { "Orin真实总线H2测量" } else { "真实mock进程空闲两入口采集/恢复；无OrinP99判据、无在途总线宣称" }}),
    )
}

async fn final_stop(
    case: &Case,
    port: &ControlPort,
    s0: &mut tokio::process::Child,
    address: &str,
    hardware_id: &str,
) -> Result<Value, String> {
    // 即使前面的日志、连接、采样或证据写入失败，也独立发出一次真实急停。
    let disarmed = send_control(port, case, s0, HookCommandKind::ArmSample, 1, 0, 0).await;
    let estop = send_control(port, case, s0, HookCommandKind::ExternalEStop, 0, 0, 0).await;
    tokio::time::sleep(Duration::from_millis(700)).await;
    let observed = async {
        let mut client = connect(address).await?;
        let state = client
            .get_state(GetStateRequest {
                hardware_id: hardware_id.to_owned(),
            })
            .await
            .map_err(|e| e.to_string())?
            .into_inner();
        if state.safety_state != 3 {
            return Err(format!("最终真实断扭未确认：{state:?}"));
        }
        client
            .set_enabled(SetEnabledCommand {
                meta: Some(command_meta(false, 2000)),
                enabled: false,
            })
            .await
            .map_err(|e| e.to_string())?;
        Ok::<_, String>(
            json!({"safety_state": state.safety_state, "state_stamp_ns": state.stamp_ns}),
        )
    }
    .await;
    let evidence = json!({"disarmed": format!("{disarmed:?}"), "estop": format!("{estop:?}"),
        "observation": format!("{observed:?}")});
    write_json(&case.directory.join("final_stop.json"), &evidence)?;
    match (disarmed, estop, observed) {
        (Ok(_), Ok(_), Ok(value)) => Ok(value),
        other => Err(format!("H2 真实收尾未完成：{other:?}")),
    }
}

pub(super) async fn run(args: &Args) -> Result<Value, String> {
    let (case, hardware_id, environment) = physical_case(args)?;
    run_instance(case, &hardware_id, &environment, true).await
}

pub(super) async fn software_probe(args: &Args) -> Result<Value, String> {
    if !matches!(args.phase, Phase::Mock | Phase::All) {
        return Err("软件采样仅属于 mock/all phase".to_owned());
    }
    let case = Case::new(&args.evidence_dir, &args.bin_dir, "runtime_sla_probe", 6)?;
    run_instance(
        case,
        "arm",
        &json!({"runtime_only": true, "hardware_only": false,
        "in_flight_exercised": false}),
        false,
    )
    .await
}

async fn run_instance(
    case: Case,
    hardware_id: &str,
    environment: &Value,
    physical: bool,
) -> Result<Value, String> {
    write_json(&case.directory.join("environment.json"), environment)?;
    let port = ControlPort::new(&case.robot_id)?;
    let (gateway_config, address) = gateway_command(&case)?;
    let mut command = isolated_command(&case.bin_dir.join("s0_daemon"));
    command
        .arg("--config")
        .arg(&case.config)
        .args(["--hardware-id", hardware_id])
        .arg("--latch-path")
        .arg(&case.latch)
        .arg("--rx-trust-dir")
        .arg(&case.trust)
        .args(["--notify-mode", "standalone"])
        .env("RUST_LOG", "info");
    let mut s0 = case.spawn("s0", command)?;
    let mut gateway = None;
    let result = async {
        gateway = Some(case.spawn("gateway", gateway_config)?);
        wait_log(&case, &mut s0, "s0", "boot_ready", Duration::from_secs(15)).await?;
        wait_log(
            &case,
            gateway.as_mut().expect("刚成功启动"),
            "gateway",
            "rr_io_threads started",
            Duration::from_secs(5),
        )
        .await?;
        let mut client = connect(&address).await?;
        measure(&case, &port, &mut s0, &mut client, hardware_id, physical).await
    }
    .await;
    let final_state = final_stop(&case, &port, &mut s0, &address, hardware_id).await;
    let gateway_stop = match gateway.as_mut() {
        Some(child) => stop(child).await,
        None => Ok(()),
    };
    let s0_stop = stop(&mut s0).await;
    match (result, final_state, gateway_stop, s0_stop) {
        (Ok(measurement), Ok(final_state), Ok(()), Ok(())) => Ok(
            json!({"environment": environment, "measurement": measurement, "final_state": final_state}),
        ),
        other => Err(format!("H2 测量或收尾失败：{other:?}")),
    }
}
