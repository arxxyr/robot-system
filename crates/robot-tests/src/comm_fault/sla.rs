//! H2：真实 S0 的两种 `EStop` 入口、事务执行权内载荷与完整物理确认时序。

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
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
#[cfg(test)]
pub(super) use robot_tests::comm_fault_rpc::confirm_and_disable;
use robot_tests::comm_fault_rpc::{ack_evidence, final_stop_rpc};

const WARMUP: usize = 100;
const SAMPLES: usize = 1000;
const MAX_ATTEMPTS: usize = (WARMUP + SAMPLES) * 10;
const MAX_SAMPLE_OWNERS: usize = MAX_ATTEMPTS * 4;
// 工具证据容量门；不改变 S0 事件环、采样期限或 SLA 预算。
const MAX_EPOCH_EVENTS: usize = TRACE_CAPACITY;
const MAX_EVENT_ROW_BYTES: usize = 256;
const MAX_RETRY_BINS: usize = 8192;
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
    let mut command = isolated_command(&case.bin_dir.join("robot-s0"));
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
        "s0": verify_artifact(&args.bin_dir, "robot-s0", &["test-comm-fault-hook", "xlerobot-real"] )?,
        "gateway": verify_artifact(&args.bin_dir, "robot-gateway", &[])?,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SampleOwner {
    attempt: usize,
    external: bool,
    in_flight: bool,
}

struct OwnerEntry {
    owner: SampleOwner,
    generation: Option<u32>,
    unique: [bool; 6],
    event_count: usize,
    formal: bool,
}

#[derive(Default)]
struct RetryHistogram {
    bins: BTreeMap<u64, usize>,
    count: usize,
}

impl RetryHistogram {
    fn observe(&mut self, value: u64) -> Result<(), String> {
        if !self.bins.contains_key(&value) && self.bins.len() >= MAX_RETRY_BINS {
            return Err("CAS 分布 distinct-bin 容量越界，禁止截断".to_owned());
        }
        let count = self.count.checked_add(1).ok_or("CAS 总计数溢出")?;
        let bin = self
            .bins
            .get(&value)
            .copied()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or("CAS 分箱计数溢出")?;
        self.bins.insert(value, bin);
        self.count = count;
        Ok(())
    }

    fn evidence(&self) -> Result<Value, String> {
        if self.count == 0 {
            return if self.bins.is_empty() {
                Ok(Value::Null)
            } else {
                Err("CAS 空集与分箱不对应".to_owned())
            };
        }
        let percentile = |fraction: usize| -> Result<u64, String> {
            let index = self
                .count
                .checked_mul(fraction)
                .ok_or("CAS 百分位索引乘法溢出")?
                .div_ceil(100)
                .saturating_sub(1);
            let mut total = 0usize;
            for (&value, &count) in &self.bins {
                total = total.checked_add(count).ok_or("CAS 分箱累计溢出")?;
                if total > index {
                    return Ok(value);
                }
            }
            Err("CAS 百分位缺少对应计数".to_owned())
        };
        let total = self
            .bins
            .values()
            .try_fold(0usize, |total, &count| total.checked_add(count))
            .ok_or("CAS 分箱累计溢出")?;
        if total != self.count || self.bins.values().any(|&count| count == 0) {
            return Err("CAS 总计数与分箱不对应".to_owned());
        }
        Ok(
            json!({"count":self.count,"p50":percentile(50)?,"p99":percentile(99)?,
            "max":self.bins.last_key_value().ok_or("CAS 分箱为空")?.0}),
        )
    }
}

#[derive(Default)]
struct DerivedCas {
    groups: BTreeMap<(bool, bool), [RetryHistogram; 3]>,
    formal_sequences: BTreeMap<(bool, bool), Vec<u64>>,
}

/// 整次测量保留 owner，不按组清理；采集原件与最终排序派生件分别保存。
struct TraceJournal {
    directory: PathBuf,
    owners: BTreeMap<u64, OwnerEntry>,
    raw: std::fs::File,
    capture: std::fs::File,
    registrations: std::fs::File,
    first_error: Option<String>,
    final_drained: bool,
    owner_limit: usize,
    epoch_limit: usize,
    derived_cas: Option<DerivedCas>,
}

fn event_row(owner: SampleOwner, event: &TraceEvent) -> String {
    format!(
        "{},{},{},{},{:?},{},{},{},{},{}\n",
        owner.attempt,
        owner.external,
        event.sample_seq,
        event.stamp_ns,
        event.kind,
        event.source,
        event.normal_seq,
        event.generation,
        event.a,
        event.b
    )
}

impl TraceJournal {
    fn new(directory: &Path) -> Result<Self, String> {
        Self::with_limits(directory, MAX_SAMPLE_OWNERS, MAX_EPOCH_EVENTS)
    }

    fn with_limits(
        directory: &Path,
        owner_limit: usize,
        epoch_limit: usize,
    ) -> Result<Self, String> {
        std::fs::create_dir(directory.join("trace_epochs")).map_err(|error| error.to_string())?;
        let new_file = |name| {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(directory.join(name))
                .map_err(|error| error.to_string())
        };
        let mut capture = new_file("events_capture.csv")?;
        write_event_header(&mut capture)?;
        Ok(Self {
            directory: directory.to_owned(),
            owners: BTreeMap::new(),
            raw: new_file("trace_drains.jsonl")?,
            capture,
            registrations: new_file("trace_owners.jsonl")?,
            first_error: None,
            final_drained: false,
            owner_limit,
            epoch_limit,
            derived_cas: None,
        })
    }

    fn remember(&mut self, result: Result<(), String>) {
        if let Err(error) = result
            && self.first_error.is_none()
        {
            self.first_error = Some(error);
        }
    }

    fn status(&self) -> Result<(), String> {
        self.first_error.clone().map_or(Ok(()), Err)
    }

    fn spool(&self, sequence: u64) -> PathBuf {
        self.directory
            .join("trace_epochs")
            .join(format!("{sequence}.csv"))
    }

    fn register(
        &mut self,
        sequence: u64,
        mode: SampleMode,
        ready: BoundarySnapshot,
        owner: SampleOwner,
    ) -> Result<(), String> {
        let validation = match mode {
            SampleMode::Disarm => Err("Disarm 不能注册为采样 owner".to_owned()),
            _ if sequence == 0 => Err("采样 owner 序号不能为零".to_owned()),
            _ if owner.in_flight != (mode == SampleMode::ArmInflight) => {
                Err("采样 owner 与实际 Arm 模式不对应".to_owned())
            }
            _ if self.owners.contains_key(&sequence) => {
                Err("采样 owner 重复注册，禁止覆盖".to_owned())
            }
            _ if self.owners.len() >= self.owner_limit => {
                Err("采样 owner 容量越界，禁止淘汰".to_owned())
            }
            _ => {
                sample_boundary_ready(sequence, mode, &ready, ready.ready_ns).and_then(|matched| {
                    if matched {
                        Ok(())
                    } else {
                        Err("owner 缺少匹配的真实 Arm/Ready".to_owned())
                    }
                })
            }
        };
        let row = json!({"sequence":sequence,"attempt":owner.attempt,"external":owner.external,
            "requested_in_flight":owner.in_flight,"mode":format!("{mode:?}"),
            "ready":boundary_evidence(ready),"registered":validation.is_ok(),"error":validation.as_ref().err()});
        let saved = writeln!(self.registrations, "{row}")
            .and_then(|()| self.registrations.flush())
            .map_err(|error| error.to_string());
        self.remember(saved);
        self.remember(validation);
        self.status()?;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.spool(sequence))
            .map_err(|error| {
                let error = format!("创建 owner spool 失败：{error}");
                self.remember(Err(error.clone()));
                error
            })?;
        self.owners.insert(
            sequence,
            OwnerEntry {
                owner,
                generation: None,
                unique: [false; 6],
                event_count: 0,
                formal: false,
            },
        );
        Ok(())
    }

    fn capture(&mut self, stage: &str, events: &[TraceEvent]) -> Result<(), String> {
        let raw_events: Vec<_> = events.iter().map(|event| {
            let owner = self.owners.get(&event.sample_seq).map(|entry| json!({
                "attempt":entry.owner.attempt,"external":entry.owner.external,
                "requested_in_flight":entry.owner.in_flight}));
            json!({"owner":owner,"event":{"sample_seq":event.sample_seq,"stamp_ns":event.stamp_ns,
                "kind":format!("{:?}",event.kind),"source":event.source,"normal_seq":event.normal_seq,
                "generation":event.generation,"a":event.a,"b":event.b}})
        }).collect();
        // 整批原字段先写入；未知 owner 不伪造 CSV 标签，仍留下完整原件并 sticky FAIL。
        let row = json!({"stage":stage,"events":raw_events});
        let saved = writeln!(self.raw, "{row}")
            .and_then(|()| self.raw.flush())
            .map_err(|error| error.to_string());
        self.remember(saved);
        let mut spools: BTreeMap<u64, String> = BTreeMap::new();
        for event in events {
            let Some(entry) = self.owners.get_mut(&event.sample_seq) else {
                self.remember(Err(format!(
                    "事件 epoch {} 未注册真实 Arm/Ready owner",
                    event.sample_seq
                )));
                continue;
            };
            let row = event_row(entry.owner, event);
            let saved = self
                .capture
                .write_all(row.as_bytes())
                .map_err(|error| error.to_string());
            spools.entry(event.sample_seq).or_default().push_str(&row);
            entry.event_count = entry.event_count.saturating_add(1);
            let validated = validate_owned_event(entry, event, self.epoch_limit).and_then(|()| {
                if row.len() <= MAX_EVENT_ROW_BYTES {
                    Ok(())
                } else {
                    Err("事件 CSV 行容量越界".to_owned())
                }
            });
            self.remember(saved);
            self.remember(validated);
        }
        // 一项归属、解析或写入错误不能跳过本批后续事件及其它 owner spool。
        for (sequence, rows) in spools {
            let saved = std::fs::OpenOptions::new()
                .append(true)
                .open(self.spool(sequence))
                .and_then(|mut file| file.write_all(rows.as_bytes()).and_then(|()| file.flush()))
                .map_err(|error| error.to_string());
            self.remember(saved);
        }
        let flushed = self.capture.flush().map_err(|error| error.to_string());
        self.remember(flushed);
        self.status()
    }

    fn route(
        &mut self,
        stage: &str,
        sequence: u64,
        current: &mut Vec<TraceEvent>,
        batch: Vec<TraceEvent>,
    ) -> Result<(), String> {
        let captured = self.capture(stage, &batch);
        // 当前解析只取本 epoch；旧 epoch 已按不可变 owner 完整保存并严格校验。
        current.extend(
            batch
                .into_iter()
                .filter(|event| event.sample_seq == sequence),
        );
        captured
    }

    fn complete(&mut self, sequence: u64, events: &[TraceEvent]) -> Result<(), String> {
        let primitive = events
            .iter()
            .find(|event| event.kind == TraceKind::PrimitiveCommitted)
            .ok_or("完成样本缺少真实 PrimitiveCommitted")?;
        let entry = self.owners.get_mut(&sequence).ok_or("完成样本缺少 owner")?;
        validate_event_membership(events, entry.owner.external, primitive.generation)?;
        entry.generation = Some(primitive.generation);
        Ok(())
    }

    fn mark_formal(
        &mut self,
        sequence: u64,
        actual_in_flight: bool,
        warmup: bool,
    ) -> Result<(), String> {
        let entry = self.owners.get_mut(&sequence).ok_or("正式样本缺少 owner")?;
        if entry.generation.is_none() {
            return Err("正式样本尚未完成严格解析".to_owned());
        }
        // 集合由原 group 的 matched/warmup 决策唯一确定，不由尾部统计重选。
        entry.formal = actual_in_flight == entry.owner.in_flight && !warmup;
        Ok(())
    }

    fn render_csv(&self) -> Result<DerivedCas, String> {
        let pending = self.directory.join("events.csv.pending");
        let mut csv = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&pending)
            .map_err(|error| error.to_string())?;
        write_event_header(&mut csv)?;
        let mut derived = DerivedCas::default();
        for (&sequence, entry) in &self.owners {
            if entry.event_count > self.epoch_limit {
                return Err("单 epoch 事件数越界；spool 保留，禁止截断派生 CSV".to_owned());
            }
            let path = self.spool(sequence);
            let metadata = path.symlink_metadata().map_err(|error| error.to_string())?;
            let byte_limit = self
                .epoch_limit
                .checked_mul(MAX_EVENT_ROW_BYTES)
                .ok_or("spool 容量计算溢出")?;
            if !metadata.file_type().is_file() || metadata.len() > byte_limit as u64 {
                return Err("spool 不是有界普通文件".to_owned());
            }
            let file = std::fs::File::open(&path).map_err(|error| error.to_string())?;
            if !file
                .metadata()
                .map_err(|error| error.to_string())?
                .file_type()
                .is_file()
            {
                return Err("打开的 spool 不是普通文件".to_owned());
            }
            let mut bytes = Vec::new();
            file.take(byte_limit as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(|error| error.to_string())?;
            if bytes.len() > byte_limit {
                return Err("spool 读取期间容量越界，原件保留".to_owned());
            }
            let text = std::str::from_utf8(&bytes).map_err(|error| error.to_string())?;
            let mut rows = Vec::new();
            for row in text.split_inclusive('\n') {
                let fields: Vec<_> = row.trim_end_matches('\n').split(',').collect();
                if !row.ends_with('\n')
                    || row.len() > MAX_EVENT_ROW_BYTES
                    || fields.len() != 10
                    || fields[0] != entry.owner.attempt.to_string()
                    || fields[1] != entry.owner.external.to_string()
                    || fields[2] != sequence.to_string()
                {
                    return Err("spool 行截断、容量或 owner 字段异常".to_owned());
                }
                rows.push((
                    fields[3]
                        .parse::<u64>()
                        .map_err(|error| error.to_string())?,
                    row,
                ));
            }
            if rows.len() != entry.event_count || rows.len() > self.epoch_limit {
                return Err("spool 行数与真实捕获计数不对应".to_owned());
            }
            // 稳定排序保留同 stamp 的原观测顺序，不改原 raw/capture/spool。
            rows.sort_by_key(|(stamp, _)| *stamp);
            let identity = (entry.owner.external, entry.owner.in_flight);
            if entry.formal {
                derived
                    .formal_sequences
                    .entry(identity)
                    .or_default()
                    .push(sequence);
            }
            for (_, row) in rows {
                csv.write_all(row.as_bytes())
                    .map_err(|error| error.to_string())?;
                if entry.formal {
                    let fields: Vec<_> = row.trim_end_matches('\n').split(',').collect();
                    let kind = match fields[4] {
                        "NormalAcquire" => Some(0),
                        "SerialClaim" => Some(1),
                        "IpcClaim" => Some(2),
                        _ => None,
                    };
                    if let Some(kind) = kind {
                        derived
                            .groups
                            .entry(identity)
                            .or_insert_with(|| std::array::from_fn(|_| RetryHistogram::default()))
                            [kind]
                            .observe(
                                fields[8]
                                    .parse::<u64>()
                                    .map_err(|error| error.to_string())?,
                            )?;
                    }
                }
            }
        }
        self.publish_csv(csv, pending)?;
        Ok(derived)
    }

    fn publish_csv(&self, mut csv: std::fs::File, pending: PathBuf) -> Result<(), String> {
        csv.flush().map_err(|error| error.to_string())?;
        let destination = self.directory.join("events.csv");
        if destination.exists() {
            return Err("禁止覆盖既有 events.csv".to_owned());
        }
        std::fs::rename(pending, destination).map_err(|error| error.to_string())
    }

    fn synchronize_cas(&self, measurement: &mut Value) -> Result<(), String> {
        self.status()?;
        let derived = self
            .derived_cas
            .as_ref()
            .ok_or("尾部完整 CSV/CAS 派生件尚未完成")?;
        let groups = measurement["groups"]
            .as_array_mut()
            .ok_or("物理组报告缺少数组")?;
        let mut evidence = Vec::new();
        if groups.len() != 4 {
            return Err("物理组报告不是四组".to_owned());
        }
        for (group, (external, in_flight)) in
            groups
                .iter_mut()
                .zip([(false, false), (false, true), (true, false), (true, true)])
        {
            if group["external"] != external || group["in_flight"] != in_flight {
                return Err("CAS 派生组身份与原四组不对应".to_owned());
            }
            let sequences = derived
                .formal_sequences
                .get(&(external, in_flight))
                .ok_or("正式样本集合未知")?;
            if sequences.len() != SAMPLES {
                return Err("CAS 派生正式样本集合不是原1000项".to_owned());
            }
            let before = group.clone();
            let histograms = derived.groups.get(&(external, in_flight));
            let fields = [
                "acquire_cas_retries",
                "serial_claim_cas_retries",
                "ipc_claim_cas_retries",
            ];
            for (index, field) in fields.iter().enumerate() {
                group[field] =
                    histograms.map_or(Ok(Value::Null), |values| values[index].evidence())?;
            }
            let mut original_non_cas = before.clone();
            let mut final_non_cas = group.clone();
            for field in fields {
                original_non_cas
                    .as_object_mut()
                    .ok_or("采集组不是对象")?
                    .remove(field);
                final_non_cas
                    .as_object_mut()
                    .ok_or("派生组不是对象")?
                    .remove(field);
            }
            if original_non_cas != final_non_cas {
                return Err("CAS 同步改变了原 SLA/population 字段".to_owned());
            }
            write_json(
                &self
                    .directory
                    .join(format!("sla_{external}_{in_flight}.json")),
                group,
            )?;
            evidence.push(
                json!({"external":external,"in_flight":in_flight,"formal_sequences":sequences,
                "non_cas_fields_unchanged":true,"capture":before,"derived":group}),
            );
        }
        write_json(
            &self.directory.join("cas_derivation.json"),
            &json!({
            "source":"真实reap后一次尾drain的完整owner spool；正式集合按原matched/warmup登记",
            "events_csv":self.directory.join("events.csv"),"retry_bin_limit":MAX_RETRY_BINS,"groups":evidence}),
        )
    }

    fn finish(
        &mut self,
        children_stopped: bool,
        drain: impl FnOnce() -> Vec<TraceEvent>,
    ) -> Result<(), String> {
        if !children_stopped || self.final_drained {
            self.remember(Err(
                "尾部 drain 要求真实 stop/reap，且整次只能执行一次".to_owned()
            ));
            return self.status();
        }
        self.final_drained = true;
        let events = drain();
        // 即使已有 sticky 失败，仍保存整批尾部事件并尝试生成完整派生件。
        let captured = self.capture("after_children_reaped", &events);
        let rendered = self.render_csv();
        self.remember(captured);
        match rendered {
            Ok(derived) => self.derived_cas = Some(derived),
            Err(error) => self.remember(Err(error)),
        }
        self.status()
    }
}

fn validate_owned_event(
    entry: &mut OwnerEntry,
    event: &TraceEvent,
    limit: usize,
) -> Result<(), String> {
    if entry.event_count > limit {
        return Err("单 epoch 事件容量越界，原件保留且样本 invalid".to_owned());
    }
    if event.a == u64::MAX
        && matches!(
            event.kind,
            TraceKind::NormalAcquire
                | TraceKind::NormalRelease
                | TraceKind::SerialClaim
                | TraceKind::IpcClaim
                | TraceKind::PrimitiveCommitted
        )
    {
        return Err("owner epoch 的 CAS 次数缺失或耗尽".to_owned());
    }
    let unique = match event.kind {
        TraceKind::TopicReceived if !entry.owner.external => Some(0),
        TraceKind::ExternalEnter if entry.owner.external => Some(0),
        TraceKind::PrimitiveCommitted => Some(1),
        TraceKind::SerialClaim if event.b == 1 => Some(2),
        TraceKind::IpcClaim if event.b == 1 => Some(3),
        TraceKind::IpcEStopCommit => Some(4),
        TraceKind::AllOffTrusted => Some(5),
        TraceKind::NormalAcquire
        | TraceKind::NormalRelease
        | TraceKind::SerialClaim
        | TraceKind::IpcClaim
        | TraceKind::SafetyStart
        | TraceKind::SafetyReturn
        | TraceKind::ConfirmReturn
        | TraceKind::InjectedDelay => None,
        _ => {
            return Err(format!(
                "owner epoch 的故障、未知 kind 或错误入口：{:?}",
                event.kind
            ));
        }
    };
    if let Some(index) = unique {
        if entry.unique[index] {
            return Err(format!("owner epoch 的 {:?} 成功/唯一事件重复", event.kind));
        }
        entry.unique[index] = true;
    }
    if let Some(generation) = entry.generation {
        validate_event_membership(&[*event], entry.owner.external, generation)?;
    }
    Ok(())
}

async fn collect(
    trace: &TracePage,
    sequence: u64,
    s0: &mut tokio::process::Child,
    events: &mut Vec<TraceEvent>,
    health: Option<&BackgroundHealth>,
    journal: &mut TraceJournal,
) -> Result<(), String> {
    let collected = collect_routed_batches(
        sequence,
        events,
        |events| {
            let batch = trace.drain(TRACE_CAPACITY);
            journal.route("collect", sequence, events, batch)
        },
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
#[cfg(test)]
pub(super) async fn collect_batches(
    sequence: u64,
    events: &mut Vec<TraceEvent>,
    mut drain: impl FnMut() -> Vec<TraceEvent>,
    dropped: impl Fn() -> u64,
    alive: impl FnMut() -> Result<bool, String>,
) -> Result<(), String> {
    collect_routed_batches(
        sequence,
        events,
        |events| {
            events.extend(drain());
            Ok(())
        },
        dropped,
        alive,
    )
    .await
}

async fn collect_routed_batches(
    sequence: u64,
    events: &mut Vec<TraceEvent>,
    mut capture: impl FnMut(&mut Vec<TraceEvent>) -> Result<(), String>,
    dropped: impl Fn() -> u64,
    mut alive: impl FnMut() -> Result<bool, String>,
) -> Result<(), String> {
    let deadline = SystemClock.now().0 + 2_000_000_000;
    loop {
        // 已消费的原始事件必须全部交还调用方保存；不能先过滤外代再宣称完整。
        capture(events)?;
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
    journal: &'a mut TraceJournal,
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
            self.journal.mark_formal(
                events[0].sample_seq,
                sample.in_flight,
                stats.completed < WARMUP,
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
                .join(format!("sla_{external}_{in_flight}_capture.json")),
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
        // 迟到事件按整次登记的旧 owner 保存，不借用当前 attempt/入口标签。
        self.journal.capture("between_samples", &unarmed)?;
        let mode = if in_flight {
            SampleMode::ArmInflight
        } else {
            SampleMode::ArmIdle
        };
        let (sequence, ready) =
            send_sample_request(self.port, self.case, self.s0, self.trace, mode).await?;
        let registered = self.journal.register(
            sequence,
            mode,
            ready,
            SampleOwner {
                attempt,
                external,
                in_flight,
            },
        );
        let mut events = Vec::new();
        let captured = async {
            // 注册失败也必须经过下面独立的 Disarm 和最外层安全 finally。
            registered?;
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
            collect(
                self.trace,
                sequence,
                self.s0,
                &mut events,
                self.health,
                self.journal,
            )
            .await
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
        let tail = self.trace.drain(TRACE_CAPACITY);
        let tail_saved = self
            .journal
            .route("after_disarm", sequence, &mut events, tail);
        events.sort_by_key(|event| event.stamp_ns);
        let disarm_ok = matches!(&disarmed, Ok((_, snapshot)) if snapshot.failure.is_none());
        let disarm_result = format!("{disarmed:?}");
        let interpreted = captured
            .and(tail_saved)
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
                self.journal.complete(sequence, &events)?;
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
            let batch = self.trace.drain(TRACE_CAPACITY);
            self.journal
                .route("wait_acquired", sequence, events, batch)?;
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

fn write_event_header(csv: &mut std::fs::File) -> Result<(), String> {
    writeln!(
        csv,
        "attempt,external,sample_seq,stamp_ns,kind,source,normal_seq,generation,a,b"
    )
    .map_err(|e| e.to_string())?;
    csv.flush().map_err(|e| e.to_string())
}

async fn measure(
    case: &Case,
    port: &ControlPort,
    s0: &mut tokio::process::Child,
    client: &mut RobotControlClient<Channel>,
    hardware_id: &str,
    kind: SlaRunKind,
    journal: &mut TraceJournal,
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
        journal,
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
    let observed = final_stop_rpc(address, hardware_id).await;
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

pub(super) async fn run(args: &Args) -> Result<Value, String> {
    let (case, hardware_id, environment) = physical_case(args)?;
    // 将共享实例编排的较大 future 留在堆上，避免沿场景分派逐层增大调用方 future。
    Box::pin(run_instance(
        case,
        &hardware_id,
        &environment,
        SlaRunKind::Physical,
    ))
    .await
}

pub(super) async fn software_probe(args: &Args) -> Result<Value, String> {
    if !matches!(args.phase, Phase::Mock | Phase::All) {
        return Err("软件采样仅属于 mock/all phase".to_owned());
    }
    let case = Case::new(&args.evidence_dir, &args.bin_dir, "runtime_sla_probe", 6)?;
    Box::pin(run_instance(
        case,
        "arm",
        &json!({"runtime_only": true, "hardware_only": false,
        "in_flight_exercised": true, "mock_lease": true, "physical_uart": false}),
        SlaRunKind::SoftwareProbe,
    ))
    .await
}

pub(super) async fn software_load(args: &Args) -> Result<Value, String> {
    if args.phase != Phase::MockH2Load || args.config.is_some() || args.hardware_id.is_some() {
        return Err("长负载仅允许无现场参数的 mock-h2-load".to_owned());
    }
    let case = Case::new(&args.evidence_dir, &args.bin_dir, "mock_h2_load", 6)?;
    Box::pin(run_instance(
        case,
        "arm",
        &json!({"runtime_only":true,"hardware_only":false,"physical_uart":false}),
        SlaRunKind::SoftwareLoad {
            cache_entries: args.h2_software_cache_entries.unwrap_or(8192),
        },
    ))
    .await
}

pub(super) struct FinalTraceObservation {
    pub installed: bool,
    pub dropped: u64,
    pub boundary: Option<BoundarySnapshot>,
}

/// 最后 Disarm 的成功回执不覆盖之后至进程停止之间的 sticky 失败。
pub(super) fn final_trace_evidence(
    children_stopped: bool,
    observation: &Result<FinalTraceObservation, String>,
) -> Value {
    let observed = observation.as_ref().ok();
    let boundary = observed.and_then(|value| value.boundary.as_ref());
    let error = match (observation, boundary) {
        _ if !children_stopped => Some("子进程未确认正常停止，最终采样状态未知".to_owned()),
        (Err(error), _) => Some(format!("最终事件环读取失败：{error}")),
        (Ok(value), _) if !value.installed => Some("最终事件环未安装".to_owned()),
        (Ok(value), _) if value.dropped != 0 => Some("最终事件环存在丢失事件".to_owned()),
        (_, None) => Some("最终采样边界快照未知".to_owned()),
        (_, Some(value)) if value.failure.is_some() || value.failure_seq != 0 => {
            Some("最终采样边界保留 sticky 失败".to_owned())
        }
        (_, Some(value))
            if value.request_seq == 0
                || value.request_seq != value.receipt_seq
                || value.request_mode != Some(SampleMode::Disarm)
                || value.mode != Some(SampleMode::Disarm)
                || value.status != SampleStatus::Disarmed
                || value.armed_seq != 0
                || value.gate_active =>
        {
            Some("最终采样边界不是完整解除状态".to_owned())
        }
        _ => None,
    };
    json!({"valid":error.is_none(), "error":error, "children_stopped":children_stopped,
        "read_error":observation.as_ref().err(),
        "installed":observed.map(|value|value.installed),
        "dropped":observed.map(|value|value.dropped),
        "boundary":boundary.map(|value|json!({
            "request_seq":value.request_seq,"receipt_seq":value.receipt_seq,
            "request_mode":format!("{:?}",value.request_mode),"mode":format!("{:?}",value.mode),
            "status":format!("{:?}",value.status),"armed_seq":value.armed_seq,
            "gate_active":value.gate_active,"failure_seq":value.failure_seq,
            "failure":format!("{:?}",value.failure)}))})
}

fn save_final_trace(
    case: &Case,
    children_stopped: bool,
    journal: &mut TraceJournal,
    measurement: Option<&mut Value>,
) -> Result<Value, String> {
    let observation = case.trace("s0").map(|trace| {
        let finished = journal.finish(children_stopped, || trace.drain(TRACE_CAPACITY));
        journal.remember(finished);
        FinalTraceObservation {
            installed: trace.installed(),
            dropped: trace.dropped(),
            boundary: trace.boundary_snapshot(),
        }
    });
    if let Some(measurement) = measurement {
        let synchronized = journal.synchronize_cas(measurement);
        journal.remember(synchronized);
    }
    let evidence = journal_trace_evidence(children_stopped, &observation, journal);
    write_json(&case.directory.join("final_trace.json"), &evidence)?;
    if evidence["valid"] != true {
        return Err(format!("最终采样有效性失败：{}", evidence["error"]));
    }
    Ok(evidence)
}

fn journal_trace_evidence(
    children_stopped: bool,
    observation: &Result<FinalTraceObservation, String>,
    journal: &TraceJournal,
) -> Value {
    let mut evidence = final_trace_evidence(children_stopped, observation);
    evidence["event_capture_error"] = json!(journal.first_error);
    evidence["final_events_drained"] = json!(journal.final_drained);
    evidence["owner_count"] = json!(journal.owners.len());
    evidence["owner_limit"] = json!(journal.owner_limit);
    evidence["epoch_event_limit"] = json!(journal.epoch_limit);
    evidence["event_row_byte_limit"] = json!(MAX_EVENT_ROW_BYTES);
    evidence["retry_distinct_bin_limit"] = json!(MAX_RETRY_BINS);
    if let Some(error) = &journal.first_error {
        evidence["valid"] = json!(false);
        evidence["error"] = json!(format!(
            "最终 trace 捕获有效性失败：{error}；边界：{}",
            evidence["error"]
        ));
    }
    evidence
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
    // Owner/证据文件必须在子进程启动前可创建，随后持续持有至真实 stop/reap 后。
    let mut journal = TraceJournal::new(&case.directory)?;
    let mut s0 = case.spawn("s0", s0_command(&case, hardware_id, physical)?)?;
    let mut gateway = None;
    let mut scheduling = Value::Null;
    let mut result = async {
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
        measure(
            &case,
            &port,
            &mut s0,
            &mut client,
            hardware_id,
            kind,
            &mut journal,
        )
        .await
    }
    .await;
    let final_state = final_stop(&case, &port, &mut s0, &address, hardware_id).await;
    let gateway_stop = match gateway.as_mut() {
        Some(child) => stop(child).await,
        None => Ok(()),
    };
    let s0_stop = stop(&mut s0).await;
    // 两个真实 stop 均已尝试；此后只读核验，不以较早 final_stop 回执代替尾部状态。
    let final_trace = save_final_trace(
        &case,
        gateway_stop.is_ok() && s0_stop.is_ok(),
        &mut journal,
        if physical { result.as_mut().ok() } else { None },
    );
    if let SlaRunKind::SoftwareLoad { cache_entries } = kind {
        let lifecycle = json!({"measurement_ok":result.is_ok() && final_trace.is_ok(),"measurement_error":result.as_ref().err(),
            "final_trace_ok":final_trace.is_ok(),"final_trace_error":final_trace.as_ref().err(),
            "final_stop_ok":final_state.is_ok(),"final_stop_error":final_state.as_ref().err(),
            "gateway_stop_ok":gateway_stop.is_ok(),"gateway_stop_error":gateway_stop.as_ref().err(),
            "s0_stop_ok":s0_stop.is_ok(),"s0_stop_error":s0_stop.as_ref().err()});
        write_json(&case.directory.join("s48_lifecycle.json"), &lifecycle)?;
        let acceptance = super::sla_cache::save_acceptance(&case, cache_entries, &lifecycle)?;
        if cache_entries == 8192 && acceptance["verdict"] != "GREEN" {
            return Err(format!("S48长负载证据未满足：{acceptance}"));
        }
    }
    match (result, final_state, gateway_stop, s0_stop, final_trace) {
        (Ok(measurement), Ok(final_state), Ok(()), Ok(()), Ok(final_trace)) => {
            let mut result = json!({"environment": environment, "measurement": measurement,
                "final_state": final_state, "final_trace": final_trace});
            if physical {
                result["rt_preflight"] = scheduling;
            }
            Ok(result)
        }
        other => Err(format!("H2 测量或收尾失败：{other:?}")),
    }
}

#[cfg(test)]
mod owner_tests {
    use super::*;
    use std::sync::mpsc;

    fn owner(attempt: usize, external: bool, in_flight: bool) -> SampleOwner {
        SampleOwner {
            attempt,
            external,
            in_flight,
        }
    }

    fn ready(sequence: u64, in_flight: bool) -> BoundarySnapshot {
        let mode = if in_flight {
            SampleMode::ArmInflight
        } else {
            SampleMode::ArmIdle
        };
        BoundarySnapshot {
            request_seq: sequence,
            request_mode: Some(mode),
            request_deadline_ns: 1000,
            receipt_seq: sequence,
            mode: Some(mode),
            status: SampleStatus::Ready,
            ready_ns: 100,
            idle_deadline_ns: if in_flight { 0 } else { 1100 },
            armed_seq: sequence,
            gate_active: !in_flight,
            gate_release: None,
            failure_seq: 0,
            failure: None,
        }
    }

    fn claim(sequence: u64, stamp: u64) -> TraceEvent {
        TraceEvent {
            sample_seq: sequence,
            stamp_ns: stamp,
            kind: TraceKind::IpcClaim,
            source: 1,
            normal_seq: 0,
            generation: 7,
            a: 0,
            b: 0,
        }
    }

    fn complete_events(sequence: u64, external: bool) -> Vec<TraceEvent> {
        [
            (
                if external {
                    TraceKind::ExternalEnter
                } else {
                    TraceKind::TopicReceived
                },
                10,
                0,
                0,
            ),
            (TraceKind::PrimitiveCommitted, 11, 2, 0),
            (TraceKind::SerialClaim, 12, 3, 1),
            (TraceKind::IpcClaim, 13, 5, 1),
            (TraceKind::SafetyStart, 14, 1, 0),
            (TraceKind::SafetyReturn, 15, 1, 1),
            (TraceKind::IpcEStopCommit, 16, 0, 0),
            (TraceKind::ConfirmReturn, 17, 0, 1),
            (TraceKind::AllOffTrusted, 17, 0, 0),
        ]
        .into_iter()
        .map(|(kind, stamp_ns, a, b)| TraceEvent {
            kind,
            stamp_ns,
            a,
            b,
            ..claim(sequence, stamp_ns)
        })
        .collect()
    }

    fn register(journal: &mut TraceJournal, sequence: u64, value: SampleOwner) {
        journal
            .register(
                sequence,
                if value.in_flight {
                    SampleMode::ArmInflight
                } else {
                    SampleMode::ArmIdle
                },
                ready(sequence, value.in_flight),
                value,
            )
            .unwrap();
    }

    fn complete(journal: &mut TraceJournal, sequence: u64, value: SampleOwner) -> Vec<TraceEvent> {
        register(journal, sequence, value);
        let events = complete_events(sequence, value.external);
        journal.capture("complete_sample", &events).unwrap();
        analyze(&events, value.external).unwrap();
        journal.complete(sequence, &events).unwrap();
        events
    }

    fn raw_events(directory: &Path) -> Vec<Value> {
        std::fs::read_to_string(directory.join("trace_drains.jsonl"))
            .unwrap()
            .lines()
            .flat_map(|line| {
                serde_json::from_str::<Value>(line).unwrap()["events"]
                    .as_array()
                    .unwrap()
                    .clone()
            })
            .collect()
    }

    #[test]
    fn paused_old_reservation_after_empty_disarm_routes_to_old_owner_across_group() {
        // 同规则受控队列 oracle；不调用或声称复现真实 RT ring 的私有暂停 hook。
        let directory = tempfile::tempdir().unwrap();
        let mut journal = TraceJournal::new(directory.path()).unwrap();
        let previous = owner(9, false, true);
        register(&mut journal, 73, previous);
        let (reserved_tx, reserved_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (published_tx, published_rx) = mpsc::channel();
        let delayed = claim(73, 14);
        let publisher = std::thread::spawn(move || {
            reserved_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            published_tx.send(delayed).unwrap();
        });
        reserved_rx.recv().unwrap();
        assert!(matches!(
            published_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        journal.capture("after_disarm", &[]).unwrap();
        register(&mut journal, 75, owner(0, true, false));
        release_tx.send(()).unwrap();
        publisher.join().unwrap();
        let next_drain = [published_rx.recv().unwrap()];
        journal.capture("between_samples", &next_drain).unwrap();
        assert_eq!(journal.owners[&73].owner, previous);
        assert_eq!(journal.owners.len(), 2);
        assert_eq!(
            std::fs::read_to_string(directory.path().join("events_capture.csv"))
                .unwrap()
                .lines()
                .last()
                .unwrap(),
            "9,false,73,14,IpcClaim,1,0,7,0,0"
        );
        assert_eq!(
            raw_events(directory.path())[0]["owner"]["requested_in_flight"],
            true
        );
        assert_eq!(
            next_drain
                .iter()
                .filter(|event| event.sample_seq == 75)
                .count(),
            0
        );
    }

    #[test]
    fn final_drain_occurs_once_only_after_real_child_wait_and_keeps_last_epoch_tail() {
        let directory = tempfile::tempdir().unwrap();
        let mut journal = TraceJournal::new(directory.path()).unwrap();
        complete(
            &mut journal,
            u64::from(u32::MAX) + 73,
            owner(1099, true, false),
        );
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let waited = child.wait().unwrap();
        assert_eq!(child.try_wait().unwrap(), Some(waited));
        let late = claim(u64::from(u32::MAX) + 73, 12);
        let mut drains = 0;
        journal
            .finish(waited.success(), || {
                drains += 1;
                vec![late]
            })
            .unwrap();
        assert_eq!(drains, 1);
        let csv = std::fs::read_to_string(directory.path().join("events.csv")).unwrap();
        assert!(csv.contains(&event_row(owner(1099, true, false), &late)));
        assert!(
            journal
                .finish(true, || {
                    drains += 1;
                    vec![]
                })
                .is_err()
        );
        assert_eq!(drains, 1);
        let directory = tempfile::tempdir().unwrap();
        let mut not_reaped = TraceJournal::new(directory.path()).unwrap();
        assert!(
            not_reaped
                .finish(false, || panic!("未reap不能drain"))
                .is_err()
        );
        assert!(!not_reaped.final_drained);
    }

    #[tokio::test]
    async fn production_route_collects_current_sample_and_preserves_legal_old_owner() {
        let directory = tempfile::tempdir().unwrap();
        let mut journal = TraceJournal::new(directory.path()).unwrap();
        complete(&mut journal, 73, owner(9, false, false));
        register(&mut journal, 75, owner(0, true, false));
        let late = claim(73, 12);
        let current_events = complete_events(75, true);
        let mut batch = vec![late];
        batch.extend(current_events.clone());
        let mut batch = Some(batch);
        let mut current = Vec::new();
        collect_routed_batches(
            75,
            &mut current,
            |events| {
                journal.route(
                    "collect",
                    75,
                    events,
                    batch.take().expect("只drain一次完整批"),
                )
            },
            || 0,
            || panic!("完整当前样本无需等待下一轮"),
        )
        .await
        .unwrap();
        assert_eq!(current, current_events);
        analyze(&current, true).unwrap();
        assert_eq!(raw_events(directory.path()).len(), 19);
        assert!(
            std::fs::read_to_string(directory.path().join("events_capture.csv"))
                .unwrap()
                .contains("9,false,73,12,IpcClaim,1,0,7,0,0")
        );
        journal.complete(75, &current).unwrap();
        journal.finish(true, Vec::new).unwrap();
    }

    #[tokio::test]
    async fn production_route_rejects_old_generation_fault_or_unknown_after_saving_whole_batch() {
        for changed in 0..3 {
            let directory = tempfile::tempdir().unwrap();
            let mut journal = TraceJournal::new(directory.path()).unwrap();
            complete(&mut journal, 73, owner(9, false, false));
            register(&mut journal, 75, owner(0, true, false));
            let mut bad = claim(73, 12);
            match changed {
                0 => bad.generation += 1,
                1 => bad.kind = TraceKind::FaultIpcCommit,
                _ => bad.sample_seq = 74,
            }
            let current_events = complete_events(75, true);
            let mut batch = vec![bad];
            batch.extend(current_events.clone());
            let mut batch = Some(batch);
            let mut current = Vec::new();
            assert!(
                collect_routed_batches(
                    75,
                    &mut current,
                    |events| { journal.route("collect", 75, events, batch.take().unwrap()) },
                    || 0,
                    || panic!("归属错误应在完整落盘后立即拒绝")
                )
                .await
                .is_err()
            );
            assert_eq!(current, current_events);
            let raw = raw_events(directory.path());
            assert_eq!(raw.len(), 19);
            assert_eq!(raw[9]["event"]["sample_seq"], bad.sample_seq);
            assert_eq!(
                std::fs::read_to_string(journal.spool(75))
                    .unwrap()
                    .lines()
                    .count(),
                9
            );
            assert!(journal.finish(true, Vec::new).is_err());
        }
    }

    #[test]
    fn late_rows_sort_stably_without_rewriting_capture_or_raw() {
        let directory = tempfile::tempdir().unwrap();
        let mut journal = TraceJournal::new(directory.path()).unwrap();
        complete(&mut journal, 73, owner(10, false, false));
        let first = TraceEvent {
            a: 21,
            ..claim(73, 12)
        };
        let second = TraceEvent {
            a: 22,
            ..claim(73, 12)
        };
        journal.capture("next_group", &[first, second]).unwrap();
        let capture = std::fs::read(directory.path().join("events_capture.csv")).unwrap();
        let raw = std::fs::read(directory.path().join("trace_drains.jsonl")).unwrap();
        journal.finish(true, Vec::new).unwrap();
        assert_eq!(
            std::fs::read(directory.path().join("events_capture.csv")).unwrap(),
            capture
        );
        assert!(
            std::fs::read(directory.path().join("trace_drains.jsonl"))
                .unwrap()
                .starts_with(&raw)
        );
        let csv = std::fs::read_to_string(directory.path().join("events.csv")).unwrap();
        let rows: Vec<_> = csv.lines().skip(1).collect();
        let stamps: Vec<u64> = rows
            .iter()
            .map(|row| row.split(',').nth(3).unwrap().parse().unwrap())
            .collect();
        assert!(stamps.windows(2).all(|pair| pair[0] <= pair[1]));
        let tied: Vec<_> = rows
            .iter()
            .filter(|row| row.split(',').nth(3) == Some("12"))
            .copied()
            .collect();
        assert!(tied[1].ends_with(",21,0") && tied[2].ends_with(",22,0"));
        assert_eq!(rows.len(), 11);
    }

    #[test]
    fn unknown_epochs_keep_entire_batch_and_make_clean_final_boundary_fail() {
        let directory = tempfile::tempdir().unwrap();
        let mut journal = TraceJournal::new(directory.path()).unwrap();
        complete(&mut journal, 73, owner(3, false, false));
        let batch = [claim(0, 18), claim(74, 19), claim(73, 20), claim(75, 21)];
        assert!(
            journal
                .capture("unknown_control_disarm_epochs", &batch)
                .is_err()
        );
        let raw = raw_events(directory.path());
        assert_eq!(raw.len(), 13);
        for (row, event) in raw[9..].iter().zip(batch) {
            assert_eq!(row["event"]["sample_seq"], event.sample_seq);
            assert_eq!(row["event"]["stamp_ns"], event.stamp_ns);
        }
        assert!(
            raw[9]["owner"].is_null() && raw[10]["owner"].is_null() && raw[12]["owner"].is_null()
        );
        assert!(
            std::fs::read_to_string(directory.path().join("events_capture.csv"))
                .unwrap()
                .contains("3,false,73,20,")
        );
        assert!(journal.finish(true, Vec::new).is_err());
        let mut boundary = ready(76, false);
        boundary.request_mode = Some(SampleMode::Disarm);
        boundary.mode = Some(SampleMode::Disarm);
        boundary.status = SampleStatus::Disarmed;
        boundary.armed_seq = 0;
        boundary.gate_active = false;
        let observation = Ok(FinalTraceObservation {
            installed: true,
            dropped: 0,
            boundary: Some(boundary),
        });
        assert_eq!(final_trace_evidence(true, &observation)["valid"], true);
        let evidence = journal_trace_evidence(true, &observation, &journal);
        assert_eq!(evidence["valid"], false);
        assert!(
            evidence["event_capture_error"]
                .as_str()
                .unwrap()
                .contains("epoch 0")
        );
    }

    #[test]
    fn completed_owner_rejects_late_fault_unknown_generation_and_duplicate_success() {
        for changed in 0..5 {
            let directory = tempfile::tempdir().unwrap();
            let mut journal = TraceJournal::new(directory.path()).unwrap();
            complete(&mut journal, 73, owner(2, false, false));
            let mut late = claim(73, 18);
            match changed {
                0 => late.kind = TraceKind::FaultIpcCommit,
                1 => late.kind = TraceKind::W4TokenBefore,
                2 => late.generation += 1,
                3 => late.b = 1,
                _ => late.a = u64::MAX,
            }
            let following = claim(73, 19);
            assert!(journal.capture("bad_late", &[late, following]).is_err());
            assert_eq!(raw_events(directory.path()).len(), 11);
            assert_eq!(
                std::fs::read_to_string(journal.spool(73))
                    .unwrap()
                    .lines()
                    .count(),
                11
            );
            assert!(journal.finish(true, Vec::new).is_err());
        }
    }

    #[test]
    fn owner_registration_requires_matching_arm_ready_and_never_overwrites_or_evicts() {
        for changed in 0..6 {
            let directory = tempfile::tempdir().unwrap();
            let mut journal =
                TraceJournal::with_limits(directory.path(), 1, MAX_EPOCH_EVENTS).unwrap();
            let previous = owner(9, false, false);
            register(&mut journal, 73, previous);
            let mut sequence = 74;
            let mut mode = SampleMode::ArmIdle;
            let mut snapshot = ready(74, false);
            match changed {
                0 => sequence = 0,
                1 => sequence = 73,
                2 => mode = SampleMode::Disarm,
                3 => snapshot.receipt_seq += 1,
                4 => snapshot.armed_seq += 1,
                _ => {}
            }
            assert!(
                journal
                    .register(sequence, mode, snapshot, owner(0, true, false))
                    .is_err()
            );
            assert_eq!(journal.owners.len(), 1);
            assert_eq!(journal.owners[&73].owner, previous);
        }
        let directory = tempfile::tempdir().unwrap();
        let mut journal = TraceJournal::new(directory.path()).unwrap();
        let mut mismatch = ready(74, true);
        mismatch.receipt_seq = 73;
        assert!(
            journal
                .register(74, SampleMode::ArmInflight, mismatch, owner(0, true, true))
                .is_err()
        );
        assert!(journal.owners.is_empty());
    }

    #[test]
    fn epoch_capacity_and_oversized_spool_fail_without_truncating_originals() {
        let directory = tempfile::tempdir().unwrap();
        let mut journal = TraceJournal::with_limits(directory.path(), 2, 2).unwrap();
        register(&mut journal, 73, owner(0, false, false));
        let batch = [claim(73, 1), claim(73, 2), claim(73, 3), claim(73, 4)];
        assert!(journal.capture("overflow", &batch).is_err());
        assert_eq!(
            std::fs::read_to_string(journal.spool(73))
                .unwrap()
                .lines()
                .count(),
            4
        );
        assert_eq!(raw_events(directory.path()).len(), 4);
        assert!(journal.finish(true, Vec::new).is_err());
        assert!(!directory.path().join("events.csv").exists());
        let directory = tempfile::tempdir().unwrap();
        let mut journal = TraceJournal::with_limits(directory.path(), 2, 2).unwrap();
        register(&mut journal, 73, owner(0, false, false));
        let blob = vec![b'x'; 2 * MAX_EVENT_ROW_BYTES + 1];
        std::fs::write(journal.spool(73), &blob).unwrap();
        assert!(journal.finish(true, Vec::new).is_err());
        assert_eq!(std::fs::read(journal.spool(73)).unwrap(), blob);
        assert!(!directory.path().join("events.csv").exists());
    }

    #[test]
    fn late_cas_changes_old_count_but_complete_spool_preserves_sla_and_population() {
        let directory = tempfile::tempdir().unwrap();
        let mut journal = TraceJournal::new(directory.path()).unwrap();
        let events = complete(&mut journal, 73, owner(100, false, false));
        let sample = analyze(&events, false).unwrap();
        let mut stats = GroupStats {
            completed: WARMUP,
            ..GroupStats::default()
        };
        stats.accept(sample, &events);
        let captured = stats.evidence(false, false);
        journal.mark_formal(73, false, false).unwrap();
        let late = claim(73, 12);
        journal.finish(true, || vec![late]).unwrap();
        let derived = journal.derived_cas.as_ref().unwrap().groups[&(false, false)][2]
            .evidence()
            .unwrap();
        assert_eq!(captured["ipc_claim_cas_retries"]["count"], 1);
        assert_eq!(derived["count"], 2, "旧采集分布应被完整CSV oracle拒绝");
        assert_ne!(captured["ipc_claim_cas_retries"], derived);
        assert_eq!(derived, distribution(&[5, 0]));
        let mut synchronized = captured.clone();
        synchronized["ipc_claim_cas_retries"] = derived;
        for field in [
            "sla_a_ns",
            "sla_b_ns",
            "samples",
            "warmup",
            "class_mismatch_retries",
            "sla_b_target_ns",
            "passed",
        ] {
            assert_eq!(captured[field], synchronized[field]);
        }
        assert_eq!(
            journal.derived_cas.as_ref().unwrap().formal_sequences[&(false, false)],
            vec![73]
        );
        assert_eq!(sample.t0_ns, 10);
        assert_eq!(sample.t1_ns, 11);
        assert_eq!(sample.all_off_ns, 17);
    }

    fn formal_oracle_events(
        sequence: u64,
        external: bool,
        actual_in_flight: bool,
    ) -> Vec<TraceEvent> {
        let mut events = complete_events(sequence, external);
        for event in &mut events {
            event.stamp_ns *= 1_000_000;
            if matches!(
                event.kind,
                TraceKind::PrimitiveCommitted | TraceKind::SerialClaim | TraceKind::IpcClaim
            ) {
                event.a = 1;
            }
            if actual_in_flight && event.kind == TraceKind::PrimitiveCommitted {
                event.b = 1;
                event.normal_seq = 37;
            }
        }
        if actual_in_flight {
            events.extend([
                TraceEvent {
                    kind: TraceKind::NormalAcquire,
                    stamp_ns: 9_000_000,
                    normal_seq: 37,
                    a: 1,
                    b: 1,
                    ..claim(sequence, 0)
                },
                TraceEvent {
                    kind: TraceKind::InjectedDelay,
                    stamp_ns: 9_500_000,
                    normal_seq: 37,
                    a: 1_000_000,
                    b: 1_000_000,
                    ..claim(sequence, 0)
                },
                TraceEvent {
                    kind: TraceKind::NormalRelease,
                    stamp_ns: 12_500_000,
                    normal_seq: 37,
                    a: 1,
                    b: 1,
                    ..claim(sequence, 0)
                },
            ]);
        }
        events.sort_by_key(|event| event.stamp_ns);
        events
    }

    fn export_formal_oracle(
        directory: &Path,
        attempts: &[Value],
        captured_groups: &[Value],
        measurement: &Value,
    ) {
        // 显式隔离软件验收可导出 fixture 给未改的 Python reporter；普通单测不留下文件。
        if let Some(destination) = std::env::var_os("H2_OWNER_SOFTWARE_FIXTURE_DIR") {
            let destination = PathBuf::from(destination);
            assert!(destination.is_absolute());
            std::fs::create_dir(&destination).unwrap();
            for name in [
                "events.csv",
                "events_capture.csv",
                "trace_drains.jsonl",
                "trace_owners.jsonl",
                "cas_derivation.json",
            ] {
                std::fs::copy(directory.join(name), destination.join(name)).unwrap();
            }
            for external in [false, true] {
                for in_flight in [false, true] {
                    for suffix in ["", "_capture"] {
                        let name = format!("sla_{external}_{in_flight}{suffix}.json");
                        std::fs::copy(directory.join(&name), destination.join(name)).unwrap();
                    }
                }
            }
            write_json(&destination.join("fixture.json"), &json!({
                "kind":"SYNTHETIC_OWNER_SOFTWARE_ORACLE","attempts":attempts,
                "captured_groups":captured_groups,"derived_measurement":measurement,
                "physical_uart":false,"scope":"确定性同规则软件fixture，不是真实RT ring或H2硬件证据"})).unwrap();
        }
    }

    fn assert_non_cas_unchanged(captured_groups: &[Value], measurement: &Value) {
        for (captured, final_group) in captured_groups
            .iter()
            .zip(measurement["groups"].as_array().unwrap())
        {
            assert_eq!(captured["ipc_claim_cas_retries"]["count"], SAMPLES);
            assert_eq!(final_group["ipc_claim_cas_retries"]["count"], SAMPLES + 1);
            assert_eq!(
                final_group["ipc_claim_cas_retries"]["max"], 1,
                "mismatch/预热显著值不得污染正式CAS"
            );
            let mut original = captured.clone();
            let mut final_non_cas = final_group.clone();
            for field in [
                "acquire_cas_retries",
                "serial_claim_cas_retries",
                "ipc_claim_cas_retries",
            ] {
                original.as_object_mut().unwrap().remove(field);
                final_non_cas.as_object_mut().unwrap().remove(field);
            }
            assert_eq!(original, final_non_cas);
        }
    }

    #[test]
    fn synchronize_cas_preserves_four_groups_with_original_thousand_formal_members() {
        let directory = tempfile::tempdir().unwrap();
        let mut journal = TraceJournal::new(directory.path()).unwrap();
        let mut groups = Vec::new();
        let mut attempts = Vec::new();
        let mut late = Vec::new();
        let mut sequence = 73u64;
        for external in [false, true] {
            for in_flight in [false, true] {
                let mut stats = GroupStats::default();
                let extra_mismatch = !external && in_flight;
                for attempt in 0..WARMUP + SAMPLES + usize::from(extra_mismatch) {
                    let matched = !(extra_mismatch && attempt == 0);
                    let actual_in_flight = in_flight && matched;
                    let value = owner(attempt, external, in_flight);
                    register(&mut journal, sequence, value);
                    let events = formal_oracle_events(sequence, external, actual_in_flight);
                    journal.capture("synthetic_formal_set", &events).unwrap();
                    let sample = analyze(&events, external).unwrap();
                    assert_eq!(sample.in_flight, actual_in_flight);
                    if actual_in_flight {
                        validate_inflight_delay(&events, &sample).unwrap();
                    }
                    journal.complete(sequence, &events).unwrap();
                    journal
                        .mark_formal(sequence, sample.in_flight, stats.completed < WARMUP)
                        .unwrap();
                    attempts.push(json!({"sequence":sequence,"attempt":attempt,"external":external,
                        "requested_in_flight":in_flight,"matched":matched,"warmup":stats.completed<WARMUP,
                        "t0_ns":sample.t0_ns,"t1_ns":sample.t1_ns,"all_off_ns":sample.all_off_ns}));
                    if matched {
                        if stats.completed == 0 {
                            late.push(TraceEvent {
                                a: 888_888,
                                ..claim(sequence, 12_000_000)
                            });
                        }
                        if stats.completed == WARMUP {
                            late.push(claim(sequence, 12_000_000));
                        }
                        stats.accept(sample, &events);
                    } else {
                        late.push(TraceEvent {
                            a: 999_999,
                            ..claim(sequence, 12_000_000)
                        });
                        stats.mismatches += 1;
                    }
                    if attempt == WARMUP + usize::from(extra_mismatch) {
                        assert!(journal.owners[&sequence].formal);
                    }
                    sequence += 2;
                }
                let group = stats.evidence(external, in_flight);
                assert_eq!(group["passed"], true);
                assert_eq!(group["class_mismatch_retries"], usize::from(extra_mismatch));
                write_json(
                    &directory
                        .path()
                        .join(format!("sla_{external}_{in_flight}_capture.json")),
                    &group,
                )
                .unwrap();
                groups.push(group);
            }
        }
        let captured_groups = groups.clone();
        let mut measurement = json!({"groups":groups});
        journal.finish(true, || late).unwrap();
        journal.synchronize_cas(&mut measurement).unwrap();
        assert_non_cas_unchanged(&captured_groups, &measurement);
        let formal = &journal.derived_cas.as_ref().unwrap().formal_sequences;
        for (identity, sequences) in formal {
            let expected: Vec<_> = attempts
                .iter()
                .filter(|row| {
                    row["external"] == identity.0
                        && row["requested_in_flight"] == identity.1
                        && row["matched"] == true
                        && row["warmup"] == false
                })
                .map(|row| row["sequence"].as_u64().unwrap())
                .collect();
            assert_eq!(sequences, &expected);
            assert_eq!(sequences.len(), SAMPLES);
        }
        assert_eq!(formal.len(), 4);
        export_formal_oracle(directory.path(), &attempts, &captured_groups, &measurement);
    }

    #[test]
    fn retry_histogram_matches_original_distribution_and_rejects_overflow() {
        for values in [
            vec![],
            vec![7],
            vec![5, 5, 5],
            vec![4, 1, 2, 3],
            vec![9, 0, 9, 4, 4],
            (0..101).rev().collect(),
        ] {
            let mut histogram = RetryHistogram::default();
            for &value in &values {
                histogram.observe(value).unwrap();
            }
            assert_eq!(histogram.evidence().unwrap(), distribution(&values));
        }
        let mut full = RetryHistogram::default();
        for value in 0..MAX_RETRY_BINS as u64 {
            full.observe(value).unwrap();
        }
        assert!(full.observe(MAX_RETRY_BINS as u64).is_err());
        assert_eq!(full.count, MAX_RETRY_BINS);
        assert_eq!(full.bins.len(), MAX_RETRY_BINS);
        let mut overflow = RetryHistogram {
            bins: BTreeMap::from([(0, usize::MAX)]),
            count: usize::MAX,
        };
        assert!(overflow.observe(0).is_err());
        assert!(overflow.evidence().is_err());
    }
}
