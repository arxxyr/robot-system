//! H2：真实 S0 的两种 `EStop` 入口、事务执行权内载荷与完整物理确认时序。

use std::collections::BTreeSet;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use iceoryx2::prelude::NodeBuilder;
use iceoryx2::service::ipc::Service;
use robot_core::time::{RobotClock, SystemClock};
use robot_msg::{build_helpers::build_estop_event, markers};
use robot_rpc::proto::robot_control_client::RobotControlClient;
use robot_rpc::proto::{ClearEStopCommand, GetStateRequest, SetEnabledCommand};
use robot_s0::comm_fault_hook::{HookCommandKind, TestCommFaultCommand};
use robot_s0::comm_fault_trace::{
    BoundarySnapshot, SampleMode, SampleStatus, TRACE_CAPACITY, TraceEvent, TraceKind, TracePage,
};
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
use super::sla_background::{Background, BackgroundHealth, RR_IN_FLIGHT_LIMIT};
use super::{Args, Phase};

const WARMUP: usize = 100;
const SAMPLES: usize = 1000;
const MAX_ATTEMPTS: usize = (WARMUP + SAMPLES) * 10;
static COMMAND_SEQ: AtomicU64 = AtomicU64::new(10_000);

const ORDINARY_CPUS: [usize; 4] = [0, 1, 2, 3];
const SERIAL_FIFO_PRIORITY: i32 = 70;
// Linux comm 只保留线程名的前 15 字节。
const SERIAL_THREAD_COMM: &str = "s0_serial_io_th";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct H2RtProfile {
    pub(super) serial_cpu: usize,
}

impl H2RtProfile {
    pub(super) fn for_hardware_id(hardware_id: &str) -> Result<Self, String> {
        let serial_cpu = match hardware_id {
            "left_arm" => 4,
            "right_arm" => 5,
            _ => return Err(format!("H2 调度配置不接受硬件身份 {hardware_id:?}")),
        };
        Ok(Self { serial_cpu })
    }
}

#[derive(Clone, Debug)]
pub(super) struct ThreadRtSnapshot {
    pub(super) tid: u32,
    pub(super) comm: Option<String>,
    pub(super) policy: Option<i32>,
    pub(super) priority: Option<i32>,
    pub(super) cpus: Option<Vec<usize>>,
}

#[derive(Clone, Debug)]
pub(super) struct ProcessRtSnapshot {
    pub(super) pid: u32,
    pub(super) tids_before: Vec<u32>,
    pub(super) tids_after: Vec<u32>,
    pub(super) threads: Vec<ThreadRtSnapshot>,
    pub(super) errors: Vec<String>,
}

impl ProcessRtSnapshot {
    fn evidence(&self) -> Value {
        let threads: Vec<_> = self
            .threads
            .iter()
            .map(|thread| {
                json!({"tid": thread.tid, "comm": thread.comm, "policy": thread.policy,
                    "priority": thread.priority, "cpus": thread.cpus})
            })
            .collect();
        json!({"pid": self.pid, "tids_before": self.tids_before,
            "tids_after": self.tids_after, "threads": threads, "errors": self.errors})
    }
}

pub(super) fn configure_physical_s0(command: &mut tokio::process::Command, profile: H2RtProfile) {
    command
        .args(["--serial-fifo", "70", "--cpu"])
        .arg(profile.serial_cpu.to_string())
        .arg("--strict-rt");
    // SAFETY：cpu_set_t 是 POD；父进程先构造完整掩码，CPU0–3 均在其容量内。
    let mut mask: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    for cpu in ORDINARY_CPUS {
        unsafe { libc::CPU_SET(cpu, &mut mask) };
    }
    // SAFETY：fork 后只在子线程执行原始系统调用并读取 errno，不分配、日志或加锁。
    // 继承掩码不是 cpuset 上限；S0 的串口线程仍由现有 --cpu 自行绑定 CPU4/5。
    unsafe {
        command.pre_exec(move || {
            match libc::sched_setaffinity(0, std::mem::size_of_val(&mask), &raw const mask) {
                0 => Ok(()),
                _ => Err(std::io::Error::last_os_error()),
            }
        });
    }
}

pub(super) fn s0_command(
    case: &Case,
    hardware_id: &str,
    physical: bool,
) -> Result<tokio::process::Command, String> {
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
    if physical {
        configure_physical_s0(&mut command, H2RtProfile::for_hardware_id(hardware_id)?);
    }
    Ok(command)
}

fn read_task_ids(pid: u32) -> Result<Vec<u32>, String> {
    let path = format!("/proc/{pid}/task");
    let entries = std::fs::read_dir(&path).map_err(|error| format!("{path}: {error}"))?;
    let mut tids = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("{path}: {error}"))?;
        let name = entry.file_name();
        let tid = name
            .to_str()
            .ok_or_else(|| format!("{path} 含非 UTF-8 线程目录"))?
            .parse::<u32>()
            .map_err(|error| format!("{path}/{}: {error}", name.display()))?;
        tids.push(tid);
    }
    tids.sort_unstable();
    Ok(tids)
}

fn read_thread_rt(pid: u32, tid: u32, errors: &mut Vec<String>) -> ThreadRtSnapshot {
    let mut thread = ThreadRtSnapshot {
        tid,
        comm: None,
        policy: None,
        priority: None,
        cpus: None,
    };
    let path = format!("/proc/{pid}/task/{tid}/comm");
    match std::fs::read_to_string(&path) {
        Ok(comm) => thread.comm = Some(comm.trim_end_matches('\n').to_owned()),
        Err(error) => errors.push(format!("{path}: {error}")),
    }
    let native_tid = match i32::try_from(tid) {
        Ok(tid) if tid > 0 => tid,
        _ => {
            errors.push(format!("PID {pid} 的 TID {tid} 无效"));
            return thread;
        }
    };
    // SAFETY：三个读取调用仅查询枚举出的正 TID；输出指针均指向有效的栈上 POD。
    let policy = unsafe { libc::sched_getscheduler(native_tid) };
    match policy {
        -1 => errors.push(format!(
            "PID {pid} TID {tid} sched_getscheduler: {}",
            std::io::Error::last_os_error()
        )),
        policy => thread.policy = Some(policy),
    }
    let mut parameter = libc::sched_param { sched_priority: 0 };
    match unsafe { libc::sched_getparam(native_tid, &raw mut parameter) } {
        0 => thread.priority = Some(parameter.sched_priority),
        _ => errors.push(format!(
            "PID {pid} TID {tid} sched_getparam: {}",
            std::io::Error::last_os_error()
        )),
    }
    let mut mask: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    match unsafe {
        libc::sched_getaffinity(native_tid, std::mem::size_of_val(&mask), &raw mut mask)
    } {
        0 => {
            thread.cpus = Some(
                (0..std::mem::size_of_val(&mask) * 8)
                    .filter(|cpu| unsafe { libc::CPU_ISSET(*cpu, &mask) })
                    .collect(),
            );
        }
        _ => errors.push(format!(
            "PID {pid} TID {tid} sched_getaffinity: {}",
            std::io::Error::last_os_error()
        )),
    }
    thread
}

/// 即使一项读取失败，也继续保存其余字段及读取后的线程集合；失败值永不填零。
pub(super) fn read_s0_threads(pid: u32) -> ProcessRtSnapshot {
    let mut snapshot = ProcessRtSnapshot {
        pid,
        tids_before: Vec::new(),
        tids_after: Vec::new(),
        threads: Vec::new(),
        errors: Vec::new(),
    };
    match read_task_ids(pid) {
        Ok(tids) => snapshot.tids_before = tids,
        Err(error) => snapshot.errors.push(format!("读取前线程集合：{error}")),
    }
    for &tid in &snapshot.tids_before {
        snapshot
            .threads
            .push(read_thread_rt(pid, tid, &mut snapshot.errors));
    }
    match read_task_ids(pid) {
        Ok(tids) => snapshot.tids_after = tids,
        Err(error) => snapshot.errors.push(format!("读取后线程集合：{error}")),
    }
    snapshot
}

fn validate_thread_inventory(snapshot: &ProcessRtSnapshot) -> Result<(), String> {
    if !snapshot.errors.is_empty() {
        return Err(format!("线程读取失败：{}", snapshot.errors.join("；")));
    }
    let before: BTreeSet<_> = snapshot.tids_before.iter().copied().collect();
    let after: BTreeSet<_> = snapshot.tids_after.iter().copied().collect();
    let observed: BTreeSet<_> = snapshot.threads.iter().map(|thread| thread.tid).collect();
    if before.is_empty()
        || before.len() != snapshot.tids_before.len()
        || after.len() != snapshot.tids_after.len()
        || before != after
        || observed.len() != snapshot.threads.len()
        || observed != before
    {
        return Err("线程集合为空、重复、读取期间变化或采集不完整".to_owned());
    }
    if snapshot.pid == 0 || !before.contains(&snapshot.pid) {
        return Err("线程集合缺少有效主线程".to_owned());
    }
    for thread in &snapshot.threads {
        if thread.comm.as_ref().is_none_or(String::is_empty)
            || thread.policy.is_none()
            || thread.priority.is_none()
            || thread.cpus.is_none()
        {
            return Err(format!("TID {} 的实际调度字段缺失", thread.tid));
        }
    }
    Ok(())
}

fn validate_thread_rt(
    thread: &ThreadRtSnapshot,
    policy: i32,
    priority: i32,
    cpus: &[usize],
) -> Result<(), String> {
    if thread.policy != Some(policy)
        || thread.priority != Some(priority)
        || thread.cpus.as_deref() != Some(cpus)
    {
        return Err(format!(
            "TID {} {:?} 实际 policy={:?} priority={:?} CPUs={:?}，要求 {policy}/{priority}/{cpus:?}",
            thread.tid, thread.comm, thread.policy, thread.priority, thread.cpus
        ));
    }
    Ok(())
}

pub(super) fn validate_s0_rt_snapshot(
    profile: H2RtProfile,
    snapshot: &ProcessRtSnapshot,
) -> Result<(), String> {
    validate_thread_inventory(snapshot)?;
    if !matches!(profile.serial_cpu, 4 | 5) {
        return Err("H2 串口目标核必须为左 4 或右 5".to_owned());
    }
    let mut serial_threads = 0;
    for thread in &snapshot.threads {
        match thread.comm.as_deref() {
            Some(SERIAL_THREAD_COMM) => {
                serial_threads += 1;
                if thread.tid == snapshot.pid {
                    return Err("S0 主线程不能冒充串口线程".to_owned());
                }
                validate_thread_rt(
                    thread,
                    libc::SCHED_FIFO,
                    SERIAL_FIFO_PRIORITY,
                    &[profile.serial_cpu],
                )?;
            }
            _ => validate_thread_rt(thread, libc::SCHED_OTHER, 0, &ORDINARY_CPUS)?,
        }
    }
    match serial_threads {
        1 => Ok(()),
        _ => Err(format!("要求恰一个真实串口线程，实际 {serial_threads} 个")),
    }
}

pub(super) fn validate_ordinary_rt_snapshot(snapshot: &ProcessRtSnapshot) -> Result<(), String> {
    validate_thread_inventory(snapshot)?;
    for thread in &snapshot.threads {
        validate_thread_rt(thread, libc::SCHED_OTHER, 0, &ORDINARY_CPUS)?;
    }
    Ok(())
}

/// 三进程全部采集、全部落证后才返回判据；此快照只证明检查时刻的实际调度。
pub(super) fn rt_preflight(
    directory: &Path,
    profile: H2RtProfile,
    tool_pid: u32,
    s0_pid: Option<u32>,
    gateway_pid: Option<u32>,
) -> Result<Value, String> {
    let started_ns = SystemClock.now().0;
    let mut processes = Vec::new();
    let mut failures = Vec::new();
    for (role, pid) in [
        ("tool", Some(tool_pid)),
        ("s0", s0_pid),
        ("gateway", gateway_pid),
    ] {
        let snapshot = read_s0_threads(pid.unwrap_or(0));
        let validation = match role {
            "s0" => validate_s0_rt_snapshot(profile, &snapshot),
            _ => validate_ordinary_rt_snapshot(&snapshot),
        };
        if let Err(error) = &validation {
            failures.push(format!("{role}: {error}"));
        }
        processes.push(json!({"role": role, "snapshot": snapshot.evidence(),
            "validation_error": validation.err()}));
    }
    let evidence = json!({"started_ns": started_ns, "finished_ns": SystemClock.now().0,
        "profile": {"serial_cpu": profile.serial_cpu, "serial_policy": libc::SCHED_FIFO,
            "serial_priority": SERIAL_FIFO_PRIORITY, "ordinary_policy": libc::SCHED_OTHER,
            "ordinary_priority": 0, "ordinary_cpus": ORDINARY_CPUS},
        "processes": processes, "passed": failures.is_empty()});
    let path = directory.join("rt_preflight.json");
    write_json(&path, &evidence)?;
    if !failures.is_empty() {
        return Err(format!(
            "H2 PX-56 调度检查失败：{}；原始记录见 {}",
            failures.join("；"),
            path.display()
        ));
    }
    Ok(evidence)
}

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
    if let Some(event) = events.iter().find(|event| {
        event.a == u64::MAX
            && matches!(
                event.kind,
                TraceKind::PrimitiveCommitted
                    | TraceKind::NormalAcquire
                    | TraceKind::NormalRelease
                    | TraceKind::SerialClaim
                    | TraceKind::IpcClaim
            )
    }) {
        return Err(format!(
            "{:?} 的 CAS 次数缺失或耗尽，样本 invalid",
            event.kind
        ));
    }
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

fn boundary_evidence(snapshot: BoundarySnapshot) -> Value {
    json!({"request_seq": snapshot.request_seq, "request_mode": format!("{:?}", snapshot.request_mode),
        "request_deadline_ns": snapshot.request_deadline_ns, "receipt_seq": snapshot.receipt_seq,
        "mode": format!("{:?}", snapshot.mode), "status": format!("{:?}", snapshot.status),
        "ready_ns": snapshot.ready_ns, "idle_deadline_ns": snapshot.idle_deadline_ns,
        "armed_seq": snapshot.armed_seq, "gate_active": snapshot.gate_active,
        "gate_release": format!("{:?}", snapshot.gate_release),
        "failure_seq": snapshot.failure_seq, "failure": format!("{:?}", snapshot.failure)})
}

/// 匹配请求与回执的真实完成事实；外层轮询期限不能掩盖已知的边界超时。
pub(super) fn sample_boundary_ready(
    sequence: u64,
    mode: SampleMode,
    snapshot: &BoundarySnapshot,
    now: u64,
) -> Result<bool, String> {
    if snapshot.request_seq != sequence {
        return Ok(false);
    }
    if snapshot.request_mode != Some(mode) {
        return Err("采样请求模式与预期不一致".to_owned());
    }
    if mode != SampleMode::Disarm && snapshot.failure.is_some() {
        return Err(format!("采样边界保留失败：{snapshot:?}"));
    }
    if snapshot.receipt_seq != sequence
        || matches!(
            snapshot.status,
            SampleStatus::Empty | SampleStatus::Pending | SampleStatus::Applying
        )
    {
        return if now >= snapshot.request_deadline_ns {
            Err("已接收的采样请求超过固定边界截止".to_owned())
        } else {
            Ok(false)
        };
    }
    if snapshot.mode != Some(mode) {
        return Err("采样回执模式与请求不一致".to_owned());
    }
    if snapshot.ready_ns >= snapshot.request_deadline_ns {
        return Err("采样边界未在固定截止前完成".to_owned());
    }
    match (mode, snapshot.status) {
        (SampleMode::Disarm, SampleStatus::Disarmed) if snapshot.armed_seq == 0 => Ok(true),
        (SampleMode::ArmIdle, SampleStatus::Ready)
            if snapshot.armed_seq == sequence
                && snapshot.gate_active
                && now < snapshot.idle_deadline_ns =>
        {
            Ok(true)
        }
        (SampleMode::ArmInflight, SampleStatus::Ready) if snapshot.armed_seq == sequence => {
            Ok(true)
        }
        _ => Err(format!("采样边界状态或有效窗口不满足请求：{snapshot:?}")),
    }
}

/// 单次真实轮询与错误留证共用入口，先读 control 提交确认再读取边界。
pub(super) fn poll_sample_request(
    sequence: u64,
    mode: SampleMode,
    last: &mut Option<BoundarySnapshot>,
    read_applied: impl FnOnce() -> Result<u64, String>,
    read_boundary: impl FnOnce() -> Option<BoundarySnapshot>,
    read_now: impl FnOnce() -> u64,
) -> Result<bool, String> {
    // control 在请求发布或拒绝后 Release 写确认字；先 Acquire 读它，
    // 再取边界快照，不能把旧快照与随后到达的新确认拼成虚假拒绝。
    let applied = read_applied()?;
    if let Some(snapshot) = read_boundary() {
        *last = Some(snapshot);
        if sample_boundary_ready(sequence, mode, &snapshot, read_now())? {
            return Ok(true);
        }
        if applied == sequence && snapshot.request_seq != sequence {
            return Err(format!("control 已处理但拒绝采样请求：{snapshot:?}"));
        }
    }
    Ok(false)
}

pub(super) async fn send_sample_request(
    port: &ControlPort,
    case: &Case,
    s0: &mut tokio::process::Child,
    trace: &TracePage,
    mode: SampleMode,
) -> Result<(u64, BoundarySnapshot), String> {
    let sequence = COMMAND_SEQ.fetch_add(1, Ordering::Relaxed);
    let started_ns = SystemClock.now().0;
    let deadline = started_ns + 3_000_000_000;
    let mut last = None;
    let result = async {
        port.send(TestCommFaultCommand {
            sequence,
            duration_ms: 0,
            count: 0,
            kind: HookCommandKind::ArmSample as u8,
            target: match mode {
                SampleMode::ArmInflight => 0,
                SampleMode::Disarm => 1,
                SampleMode::ArmIdle => 2,
            },
            error: 0,
            reserved: [0; 5],
        })?;
        loop {
            if poll_sample_request(
                sequence,
                mode,
                &mut last,
                || {
                    case.observation("s0")
                        .map(|observation| observation.command_applied)
                },
                || trace.boundary_snapshot(),
                || SystemClock.now().0,
            )? {
                return last
                    .map(|snapshot| (sequence, snapshot))
                    .ok_or_else(|| "采样完成却缺少对应边界快照".to_owned());
            }
            if let Some(status) = s0.try_wait().map_err(|error| error.to_string())? {
                return Err(format!("等待采样边界时 S0 退出：{status}"));
            }
            if SystemClock.now().0 >= deadline {
                return Err(format!("采样请求 {sequence} 未取得对应真实边界回执"));
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }
    .await;
    let evidence = json!({"sequence": sequence, "mode": format!("{mode:?}"),
        "started_ns": started_ns, "finished_ns": SystemClock.now().0,
        "snapshot": last.map(boundary_evidence), "error": result.as_ref().err()});
    let mut journal = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(case.directory.join("boundary.jsonl"))
        .map_err(|error| error.to_string())?;
    writeln!(journal, "{evidence}").map_err(|error| error.to_string())?;
    journal.flush().map_err(|error| error.to_string())?;
    result
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

fn check_background(health: Option<&BackgroundHealth>) -> Result<(), String> {
    health.map_or(Ok(()), BackgroundHealth::check)
}

pub(super) fn ack_evidence(
    response: Result<robot_rpc::proto::Ack, tonic::Status>,
    request_id: &str,
    dry_run: bool,
) -> Value {
    match response {
        Ok(ack) => {
            json!({"ok":ack.request_id == request_id && ack.dry_run == dry_run && ack.cleared_at_ns != 0,
            "kind":"ack","request_id":ack.request_id,"dry_run":ack.dry_run,"cleared_at_ns":ack.cleared_at_ns})
        }
        Err(error) => {
            json!({"ok":false,"kind":"status","code":format!("{:?}",error.code()),"message":error.message()})
        }
    }
}

/// 两个 future 直到各自步骤才被 poll。首错不取消已经发出的 live 动作。
pub(super) async fn recover_writes<C, E>(
    health: Option<&BackgroundHealth>,
    writes: &mut Vec<Value>,
    clear: (robot_rpc::proto::CommandMeta, C),
    enable: (robot_rpc::proto::CommandMeta, E),
) -> Result<(), String>
where
    C: std::future::Future<Output = Result<robot_rpc::proto::Ack, tonic::Status>>,
    E: std::future::Future<Output = Result<robot_rpc::proto::Ack, tonic::Status>>,
{
    check_background(health)?;
    let started = SystemClock.now().0;
    let response = ack_evidence(clear.1.await, &clear.0.request_id, false);
    writes.push(json!({"method":"ClearEStop","request_id":clear.0.request_id,"idempotency_key":clear.0.idempotency_key,
        "started_ns":started,"finished_ns":SystemClock.now().0,"response":response,"ok":response["ok"]}));
    if response["ok"] != true {
        return Err(format!("真实 ClearEStop 失败：{response}"));
    }
    check_background(health)?;
    let started = SystemClock.now().0;
    let response = ack_evidence(enable.1.await, &enable.0.request_id, false);
    writes.push(json!({"method":"SetEnabled(true)","request_id":enable.0.request_id,"idempotency_key":enable.0.idempotency_key,
        "started_ns":started,"finished_ns":SystemClock.now().0,"response":response,"ok":response["ok"]}));
    if response["ok"] != true {
        return Err(format!("真实 SetEnabled 失败：{response}"));
    }
    check_background(health)
}

async fn recover(
    client: &mut RobotControlClient<Channel>,
    hardware_id: &str,
    health: Option<&BackgroundHealth>,
    writes: &mut Vec<Value>,
) -> Result<(), String> {
    check_background(health)?;
    // 让上一轮的真实确认与 L0 连续窗口完成；不通过观测页写入许可。
    tokio::time::sleep(Duration::from_millis(600)).await;
    check_background(health)?;
    let mut clear_meta = command_meta(false, 2000);
    "maintainer-fake-token-001"
        .clone_into(&mut clear_meta.auth.as_mut().expect("构造器含 auth").token);
    let enable_meta = command_meta(false, 2000);
    let clear_command = ClearEStopCommand {
        meta: Some(clear_meta.clone()),
        confirmation: "H2-supervised-sample".to_owned(),
    };
    let enable_command = SetEnabledCommand {
        meta: Some(enable_meta.clone()),
        enabled: true,
    };
    let mut clear_client = client.clone();
    let mut enable_client = client.clone();
    recover_writes(
        health,
        writes,
        (clear_meta, async move {
            clear_client
                .clear_e_stop(clear_command)
                .await
                .map(tonic::Response::into_inner)
        }),
        (enable_meta, async move {
            enable_client
                .set_enabled(enable_command)
                .await
                .map(tonic::Response::into_inner)
        }),
    )
    .await?;
    let deadline = SystemClock.now().0 + 1_000_000_000;
    loop {
        check_background(health)?;
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
    health: Option<&BackgroundHealth>,
) -> Result<(), String> {
    let collected = collect_batches(
        sequence,
        events,
        || trace.drain(TRACE_CAPACITY),
        || trace.dropped(),
        || {
            check_background(health)?;
            s0.try_wait()
                .map(|status| status.is_none())
                .map_err(|error| error.to_string())
        },
    )
    .await;
    collected.and_then(|()| check_background(health))
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
    health: Option<&'a BackgroundHealth>,
    writes: &'a mut Vec<Value>,
}

impl SlaRunner<'_> {
    async fn software_load_samples(
        &mut self,
        heartbeat: &HeartbeatFeed,
        load_start: u64,
    ) -> Result<Vec<Value>, String> {
        let health = self.health.expect("长负载已启动背景");
        let mut recoveries = 0;
        loop {
            health.check()?;
            heartbeat.verify_running()?;
            let elapsed = SystemClock.now().0.saturating_sub(load_start);
            let live_successes = self.writes.iter().filter(|row| row["ok"] == true).count();
            let successes =
                health.successful() + u64::try_from(live_successes).map_err(|e| e.to_string())?;
            if elapsed >= LOAD_MIN_NS && successes > 8192 {
                return Ok(vec![
                    json!({"scope":"无UART软件负载","elapsed_ns":elapsed,"recoveries":recoveries,
                        "successful_unique_writes_before_drain":successes,"minimum_ns":LOAD_MIN_NS,"maximum_ns":LOAD_MAX_NS}),
                ]);
            }
            if elapsed >= LOAD_MAX_NS {
                return Err("600s内未取得至少360s且成功唯一新写超过8192的证据".to_owned());
            }
            let (sample, events) = self.attempt(recoveries, true, false).await?;
            self.record_attempt(&json!({"software_load":true,"attempt":recoveries,"sequence":events.first().map(|event|event.sample_seq),
                    "event_count":events.len(),"all_off_ns":sample.all_off_ns,"matched":!sample.in_flight}))?;
            if sample.in_flight {
                return Err("软件长负载 idle 边界未匹配".to_owned());
            }
            recoveries += 1;
        }
    }

    async fn software_samples(&mut self) -> Result<Vec<Value>, String> {
        let mut samples = Vec::new();
        for external in [false, true] {
            for in_flight in [false, true] {
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
                let mut completed = 0;
                for attempt in 0..32 {
                    let (sample, events) = self.attempt(attempt, external, in_flight).await?;
                    if sample.in_flight != in_flight {
                        self.record_attempt(&json!({"external": external, "attempt": attempt,
                        "requested_in_flight": in_flight, "matched": false,
                        "epoch": events[0].sample_seq}))?;
                        continue;
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
                    let evidence = json!({"external": external, "cycle": completed, "attempt": attempt,
                    "epoch": events[0].sample_seq, "t0_ns": sample.t0_ns, "t1_ns": sample.t1_ns,
                    "all_off_ns": sample.all_off_ns, "real_rpc_recovery_verified": true,
                    "mock_safety_call_ns": safety_durations, "in_flight": in_flight,
                    "requested_in_flight": in_flight, "matched": true, "mock_lease": true});
                    self.record_attempt(&evidence)?;
                    samples.push(evidence);
                    completed += 1;
                    if completed == 2 {
                        break;
                    }
                }
                if completed != 2 {
                    return Err("mock 四组采样未在有界次数内取得每组两项完整匹配样本".to_owned());
                }
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
        recover(self.client, self.hardware_id, self.health, self.writes).await?;
        let unarmed = self.trace.drain(TRACE_CAPACITY);
        // 上一 epoch 必须已完整消费并解除采样；迟到事件同样保留，不静默丢弃。
        save_events(self.csv, attempt, external, &unarmed)?;
        let (sequence, _) = send_sample_request(
            self.port,
            self.case,
            self.s0,
            self.trace,
            if in_flight {
                SampleMode::ArmInflight
            } else {
                SampleMode::ArmIdle
            },
        )
        .await?;
        let mut events = Vec::new();
        let captured = async {
            if in_flight {
                self.wait_acquired(sequence, &mut events).await?;
            }
            check_background(self.health)?;
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
            collect(self.trace, sequence, self.s0, &mut events, self.health).await
        }
        .await;
        let disarmed = send_sample_request(
            self.port,
            self.case,
            self.s0,
            self.trace,
            SampleMode::Disarm,
        )
        .await;
        // 回执确认真实 lease 完整退场后才收尾；失败也保存已消费的全部原始事件。
        events.extend(self.trace.drain(TRACE_CAPACITY));
        events.sort_by_key(|event| event.stamp_ns);
        save_events(self.csv, attempt, external, &events)?;
        let disarm_ok = matches!(&disarmed, Ok((_, snapshot)) if snapshot.failure.is_none());
        let disarm_result = format!("{disarmed:?}");
        let interpreted = captured
            .and(disarmed.and_then(|(_, snapshot)| match snapshot.failure {
                Some(failure) => Err(format!(
                    "采样边界保留失败：seq={} {failure:?}",
                    snapshot.failure_seq
                )),
                None => Ok(()),
            }))
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
                    "disarm_ok": disarm_ok, "disarm_result": disarm_result,
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
            check_background(self.health)?;
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

#[derive(Clone, Copy, Debug)]
pub(super) enum SlaRunKind {
    Physical,
    SoftwareProbe,
    SoftwareLoad { cache_entries: usize },
}

pub(super) fn configure_h2_gateway(
    command: &mut tokio::process::Command,
    kind: SlaRunKind,
) -> Result<Option<usize>, String> {
    let entries = match kind {
        SlaRunKind::Physical => Some(8192),
        SlaRunKind::SoftwareProbe => None,
        SlaRunKind::SoftwareLoad {
            cache_entries: entries @ (4096 | 8192),
        } => Some(entries),
        SlaRunKind::SoftwareLoad { .. } => return Err("软件缓存对照仅允许4096或8192".to_owned()),
    };
    if let Some(entries) = entries {
        command
            .args(["--idempotency-ttl-secs", "300", "--idempotency-max-entries"])
            .arg(entries.to_string());
    }
    if matches!(kind, SlaRunKind::SoftwareLoad { .. }) {
        command
            .env("ROBOT_TEST_PE_CACHE_OBSERVE", "1")
            .env("ROBOT_LOG_FORMAT", "json");
    }
    Ok(entries)
}

const LOAD_MIN_NS: u64 = 360_000_000_000;
const LOAD_MAX_NS: u64 = 600_000_000_000;

fn create_event_csv(directory: &Path) -> Result<std::fs::File, String> {
    let mut csv = std::fs::File::create(directory.join("events.csv")).map_err(|e| e.to_string())?;
    writeln!(
        csv,
        "attempt,external,sample_seq,stamp_ns,kind,source,normal_seq,generation,a,b"
    )
    .map_err(|e| e.to_string())?;
    Ok(csv)
}

async fn measure(
    case: &Case,
    port: &ControlPort,
    s0: &mut tokio::process::Child,
    client: &mut RobotControlClient<Channel>,
    hardware_id: &str,
    kind: SlaRunKind,
) -> Result<Value, String> {
    let physical = matches!(kind, SlaRunKind::Physical);
    let long_load = matches!(kind, SlaRunKind::SoftwareLoad { .. });
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
    let mut csv = create_event_csv(&case.directory)?;
    let attempts =
        std::fs::File::create(case.directory.join("attempts.jsonl")).map_err(|e| e.to_string())?;
    send_control(port, case, s0, HookCommandKind::ExternalEStop, 0, 0, 0).await?;
    let background = if physical || long_load {
        Some(Background::start(
            client.clone(),
            long_load.then_some(LOAD_MAX_NS),
        )?)
    } else {
        None
    };
    let load_start = background.as_ref().map_or(0, Background::started_ns);
    let health = background.as_ref().map(Background::health);
    let mut writes = Vec::new();
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
        health: health.as_ref(),
        writes: &mut writes,
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
        } else if long_load {
            groups = runner.software_load_samples(&heartbeat, load_start).await?;
        } else {
            heartbeat.verify_running()?;
            groups = runner.software_samples().await?;
        }
        Ok::<(), String>(())
    }
    .await;
    let background = match background {
        Some(background) => background.finish().await,
        None => Value::Null,
    };
    write_json(&case.directory.join("background.json"), &background)?;
    write_json(&case.directory.join("recovery_writes.json"), &json!(writes))?;
    if long_load {
        save_load_summary(
            &case.directory,
            &background,
            &writes,
            &groups,
            load_start,
            measurement.as_ref().err(),
            trace.dropped(),
        )?;
    }
    measurement?;
    if (physical || long_load)
        && (background["valid_responses"] != true || background["valid_cadence"] != true)
    {
        return Err("真实 RR 背景负载发生错误或未维持 20/s 调度，完整记录已保留".to_owned());
    }
    if trace.dropped() != 0 {
        return Err("累计事件丢失，整次 H2 运行无效".to_owned());
    }
    Ok(
        json!({"groups": groups, "background": background, "trace_dropped": trace.dropped(),
        "events_csv": case.directory.join("events.csv"), "hardware_only": physical, "runtime_only": !physical, "in_flight_exercised": !long_load,
        "mock_lease": !physical, "external_entry_exercised":true,"topic_entry_exercised":!long_load,"idle_exercised":true,
        "scope": if physical { "Orin真实总线H2测量" } else if long_load { "真实mock三进程长期20/s与恢复写；无UART及物理P99宣称" } else { "真实mock进程两入口×idle/inflight调用lease采集/恢复；无UART与OrinP99宣称" }}),
    )
}

fn save_load_summary(
    directory: &Path,
    background: &Value,
    writes: &[Value],
    groups: &[Value],
    load_start: u64,
    measurement_error: Option<&String>,
    trace_dropped: u64,
) -> Result<(), String> {
    let mut keys = BTreeSet::new();
    for row in background["rr_requests"]
        .as_array()
        .into_iter()
        .flatten()
        .chain(writes.iter())
    {
        if row["ok"] == true {
            let key = row["idempotency_key"]
                .as_str()
                .ok_or("成功写缺少唯一幂等键")?;
            if !keys.insert(key) {
                return Err("长负载存在重复幂等键，不能作为新写证明".to_owned());
            }
        }
    }
    write_json(
        &directory.join("load_summary.json"),
        &json!({"successful_unique_keys":keys,
            "successful_unique_count":keys.len(),"minimum_ns":LOAD_MIN_NS,"maximum_ns":LOAD_MAX_NS,
            "started_ns":load_start,"stop_ns":background["stop_ns"],"groups":groups,
            "measurement_error":measurement_error,"trace_dropped":trace_dropped,"no_uart":true,"physical_p99_claim":false}),
    )?;
    Ok(())
}

async fn final_stop(
    case: &Case,
    port: &ControlPort,
    s0: &mut tokio::process::Child,
    address: &str,
    hardware_id: &str,
) -> Result<Value, String> {
    // 即使前面的日志、连接、采样或证据写入失败，也独立发出一次真实急停。
    let estop = send_control(port, case, s0, HookCommandKind::ExternalEStop, 0, 0, 0).await;
    let disarmed = match case.trace("s0") {
        Ok(trace) => send_sample_request(port, case, s0, &trace, SampleMode::Disarm).await,
        Err(error) => Err(error),
    };
    tokio::time::sleep(Duration::from_millis(700)).await;
    let observed = match connect(address).await {
        Ok(mut client) => {
            let meta = command_meta(false, 2000);
            let mut disable_client = client.clone();
            let command = SetEnabledCommand {
                meta: Some(meta.clone()),
                enabled: false,
            };
            let mut observed = confirm_and_disable(
                async move {
                    client
                        .get_state(GetStateRequest {
                            hardware_id: hardware_id.to_owned(),
                        })
                        .await
                        .map(tonic::Response::into_inner)
                },
                async move {
                    disable_client
                        .set_enabled(command)
                        .await
                        .map(tonic::Response::into_inner)
                },
                &meta.request_id,
            )
            .await;
            observed["disable"]["request"] = json!({"request_id":meta.request_id,
                "idempotency_key":meta.idempotency_key,"dry_run":false,"enabled":false});
            observed
        }
        Err(error) => json!({"ok":false,"get_state":{"attempted":false,"ok":false,"error":error},
            "disable":{"attempted":false,"ok":false,"reason":"连接失败"}}),
    };
    let evidence = json!({"ok":disarmed.is_ok() && estop.is_ok() && observed["ok"] == true,
        "disarmed":format!("{disarmed:?}"),"estop":format!("{estop:?}"),
        "get_state":observed["get_state"],"disable":observed["disable"]});
    write_json(&case.directory.join("final_stop.json"), &evidence)?;
    if evidence["ok"] == true {
        Ok(evidence)
    } else {
        Err(format!("H2 真实收尾未完成：{evidence}"))
    }
}

/// 真实收尾使用同一顺序接缝：状态 3 成功并不代表随后 Disable 成功。
pub(super) async fn confirm_and_disable<G, D>(get_state: G, disable: D, request_id: &str) -> Value
where
    G: std::future::Future<Output = Result<robot_rpc::proto::RobotState, tonic::Status>>,
    D: std::future::Future<Output = Result<robot_rpc::proto::Ack, tonic::Status>>,
{
    let observed = match get_state.await {
        Ok(state) => json!({"attempted":true,"ok":state.safety_state == 3,
            "safety_state":state.safety_state,"state_stamp_ns":state.stamp_ns}),
        Err(error) => {
            json!({"attempted":true,"ok":false,"code":format!("{:?}",error.code()),"error":error.message()})
        }
    };
    let disabled = if observed["ok"] == true {
        let started = SystemClock.now().0;
        let mut value = ack_evidence(disable.await, request_id, false);
        value["attempted"] = json!(true);
        value["started_ns"] = json!(started);
        value["finished_ns"] = json!(SystemClock.now().0);
        value
    } else {
        json!({"attempted":false,"ok":false,"reason":"未取得真实状态3确认"})
    };
    json!({"ok":observed["ok"] == true && disabled["ok"] == true,"get_state":observed,"disable":disabled})
}

pub(super) async fn run(args: &Args) -> Result<Value, String> {
    let (case, hardware_id, environment) = physical_case(args)?;
    run_instance(case, &hardware_id, &environment, SlaRunKind::Physical).await
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
        "in_flight_exercised": true, "mock_lease": true, "physical_uart": false}),
        SlaRunKind::SoftwareProbe,
    )
    .await
}

pub(super) async fn software_load(args: &Args) -> Result<Value, String> {
    if args.phase != Phase::MockH2Load || args.config.is_some() || args.hardware_id.is_some() {
        return Err("长负载仅允许无现场参数的 mock-h2-load".to_owned());
    }
    let case = Case::new(&args.evidence_dir, &args.bin_dir, "mock_h2_load", 6)?;
    run_instance(
        case,
        "arm",
        &json!({"runtime_only":true,"hardware_only":false,"physical_uart":false}),
        SlaRunKind::SoftwareLoad {
            cache_entries: args.h2_software_cache_entries.unwrap_or(8192),
        },
    )
    .await
}

async fn run_instance(
    case: Case,
    hardware_id: &str,
    environment: &Value,
    kind: SlaRunKind,
) -> Result<Value, String> {
    let physical = matches!(kind, SlaRunKind::Physical);
    let mut environment = environment.clone();
    let port = ControlPort::new(&case.robot_id)?;
    let (mut gateway_config, address) = gateway_command(&case)?;
    let entries = configure_h2_gateway(&mut gateway_config, kind)?;
    environment["gateway_idempotency_max_entries"] = json!(entries.unwrap_or(4096));
    environment["gateway_idempotency_ttl_secs"] = json!(300);
    environment["background_tracked_limit"] = json!(RR_IN_FLIGHT_LIMIT);
    environment["wire_active_limit_unchanged"] = json!(4);
    environment["s48_cache_observation"] = json!(matches!(kind, SlaRunKind::SoftwareLoad { .. }));
    environment["gateway_arguments"] = json!(
        gateway_config
            .as_std()
            .get_args()
            .map(|value| value.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
    );
    write_json(&case.directory.join("environment.json"), &environment)?;
    let mut s0 = case.spawn("s0", s0_command(&case, hardware_id, physical)?)?;
    let mut gateway = None;
    let mut scheduling = Value::Null;
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
        if physical {
            scheduling = rt_preflight(
                &case.directory,
                H2RtProfile::for_hardware_id(hardware_id)?,
                std::process::id(),
                s0.id(),
                gateway.as_ref().and_then(tokio::process::Child::id),
            )?;
        }
        measure(&case, &port, &mut s0, &mut client, hardware_id, kind).await
    }
    .await;
    let final_state = final_stop(&case, &port, &mut s0, &address, hardware_id).await;
    let gateway_stop = match gateway.as_mut() {
        Some(child) => stop(child).await,
        None => Ok(()),
    };
    let s0_stop = stop(&mut s0).await;
    if let SlaRunKind::SoftwareLoad { cache_entries } = kind {
        let lifecycle = json!({"measurement_ok":result.is_ok(),"measurement_error":result.as_ref().err(),
            "final_stop_ok":final_state.is_ok(),"final_stop_error":final_state.as_ref().err(),
            "gateway_stop_ok":gateway_stop.is_ok(),"gateway_stop_error":gateway_stop.as_ref().err(),
            "s0_stop_ok":s0_stop.is_ok(),"s0_stop_error":s0_stop.as_ref().err()});
        write_json(&case.directory.join("s48_lifecycle.json"), &lifecycle)?;
        let acceptance = super::sla_cache::save_acceptance(&case, cache_entries, &lifecycle)?;
        if cache_entries == 8192 && acceptance["verdict"] != "GREEN" {
            return Err(format!("S48长负载证据未满足：{acceptance}"));
        }
    }
    match (result, final_state, gateway_stop, s0_stop) {
        (Ok(measurement), Ok(final_state), Ok(()), Ok(())) => {
            let mut result = json!({"environment": environment, "measurement": measurement,
                "final_state": final_state});
            if physical {
                result["rt_preflight"] = scheduling;
            }
            Ok(result)
        }
        other => Err(format!("H2 测量或收尾失败：{other:?}")),
    }
}
