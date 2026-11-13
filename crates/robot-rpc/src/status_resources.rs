//! 独立普通线程定期读取Linux procfs；HTTP仅查询同一份最新缓存。

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use axum::extract::State;
use axum::{Json, Router};
use robot_core::time::RobotClock;
use serde::Serialize;
use tokio::sync::watch;

const INTERVAL_MS: u64 = 2000;
const STALE_AFTER_MS: u64 = 6000;
const MAX_PROC_BYTES: u64 = 256 * 1024;

#[derive(Clone, Serialize)]
pub(super) struct Snapshot {
    status: &'static str,
    error: Option<String>,
    stamp_ns: Option<String>,
    age_ms: Option<u64>,
    interval_ms: u64,
    stale_after_ms: u64,
    cpu: Option<Cpu>,
    memory: Option<Memory>,
    tasks: Option<Tasks>,
    context_switches: Option<ContextSwitches>,
    networks: Vec<Network>,
    web_process: Option<WebProcess>,
}

#[derive(Clone, Serialize)]
struct Cpu {
    usage_percent: Option<f64>,
    logical_cores: u32,
    load_average: [f64; 3],
}

#[derive(Clone, Serialize)]
// 对外字段明确以字节为单位，保留同一wire命名而不隐去单位。
#[allow(clippy::struct_field_names)]
struct Memory {
    total_bytes: u64,
    available_bytes: u64,
    used_bytes: u64,
    swap_total_bytes: u64,
    swap_used_bytes: u64,
}

#[derive(Clone, Serialize)]
struct Tasks {
    total: u64,
    running: u64,
}

#[derive(Clone, Serialize)]
struct ContextSwitches {
    total: String,
    per_second: Option<f64>,
}

#[derive(Clone, Serialize)]
struct Network {
    name: String,
    rx_bytes: String,
    tx_bytes: String,
    rx_bytes_per_second: Option<f64>,
    tx_bytes_per_second: Option<f64>,
}

#[derive(Clone, Serialize)]
struct WebProcess {
    threads: u64,
    resident_bytes: u64,
}

impl Snapshot {
    fn unavailable(error: impl Into<String>) -> Self {
        Self {
            status: "unavailable",
            error: Some(error.into()),
            stamp_ns: None,
            age_ms: None,
            interval_ms: INTERVAL_MS,
            stale_after_ms: STALE_AFTER_MS,
            cpu: None,
            memory: None,
            tasks: None,
            context_switches: None,
            networks: Vec::new(),
            web_process: None,
        }
    }

    fn at_time(&self, now_ns: u64) -> Self {
        let Some(stamp_ns) = self
            .stamp_ns
            .as_ref()
            .and_then(|stamp| stamp.parse::<u64>().ok())
        else {
            return self.clone();
        };
        let Some(age_ns) = now_ns.checked_sub(stamp_ns) else {
            return Self::unavailable("资源源时间戳晚于本机时钟");
        };
        let mut view = if age_ns > STALE_AFTER_MS * 1_000_000 {
            Self::unavailable("资源缓存已陈旧")
        } else {
            self.clone()
        };
        view.stamp_ns = Some(stamp_ns.to_string());
        view.age_ms = Some(age_ns / 1_000_000);
        view
    }
}

#[derive(Clone)]
pub(super) struct ResourcesState {
    latest: Arc<Mutex<Snapshot>>,
    clock: Arc<dyn RobotClock>,
}

impl ResourcesState {
    pub(super) fn new(clock: Arc<dyn RobotClock>) -> Self {
        Self {
            latest: Arc::new(Mutex::new(Snapshot::unavailable("等待首次资源采样"))),
            clock,
        }
    }

    pub(super) fn start(
        &self,
        shutdown: watch::Receiver<bool>,
    ) -> std::io::Result<JoinHandle<Result<(), String>>> {
        self.start_with_reader(shutdown, read_system)
    }

    fn start_with_reader(
        &self,
        shutdown: watch::Receiver<bool>,
        mut read: impl FnMut() -> Result<RawSample, String> + Send + 'static,
    ) -> std::io::Result<JoinHandle<Result<(), String>>> {
        let state = self.clone();
        std::thread::Builder::new()
            .name("robot-resources".into())
            .spawn(move || {
                let mut previous = None;
                let mut next_ns = 0;
                while !*shutdown.borrow() && shutdown.has_changed().is_ok() {
                    let now_ns = state.clock.now().0;
                    if now_ns < next_ns {
                        std::thread::park_timeout(Duration::from_nanos(next_ns - now_ns));
                        continue;
                    }
                    {
                        let result = read();
                        let stamp_ns = state.clock.now().0;
                        next_ns = stamp_ns.saturating_add(INTERVAL_MS * 1_000_000);
                        let view = match result {
                            Ok(raw) => {
                                let view = raw.view(stamp_ns, previous.as_ref());
                                previous = Some((stamp_ns, raw));
                                view
                            }
                            Err(error) => {
                                previous = None;
                                Snapshot::unavailable(error)
                            }
                        };
                        *state.latest.lock().map_err(|_| "资源缓存锁已损坏")? = view;
                    }
                }
                Ok(())
            })
    }

    fn snapshot(&self) -> Snapshot {
        match self.latest.lock() {
            Ok(latest) => latest.at_time(self.clock.now().0),
            Err(_) => Snapshot::unavailable("资源缓存锁已损坏"),
        }
    }
}

#[derive(Clone)]
struct CpuCounters {
    ticks: [u64; 8],
    logical_cores: u32,
}

#[derive(Clone)]
struct NetworkCounters {
    rx: u64,
    tx: u64,
}

#[derive(Clone)]
struct RawSample {
    cpu: CpuCounters,
    load: [f64; 3],
    memory: Memory,
    tasks: Tasks,
    context_switches: u64,
    networks: BTreeMap<String, NetworkCounters>,
    web_process: WebProcess,
}

impl RawSample {
    fn view(&self, stamp_ns: u64, previous: Option<&(u64, Self)>) -> Snapshot {
        let previous = previous.filter(|(stamp, _)| stamp_ns > *stamp);
        let elapsed_ns = previous.map(|(stamp, _)| stamp_ns - stamp);
        let prior = previous.map(|(_, raw)| raw);
        Snapshot {
            status: "ok",
            error: None,
            stamp_ns: Some(stamp_ns.to_string()),
            age_ms: Some(0),
            interval_ms: INTERVAL_MS,
            stale_after_ms: STALE_AFTER_MS,
            cpu: Some(Cpu {
                usage_percent: prior.and_then(|prior| cpu_usage(&self.cpu, &prior.cpu)),
                logical_cores: self.cpu.logical_cores,
                load_average: self.load,
            }),
            memory: Some(self.memory.clone()),
            tasks: Some(self.tasks.clone()),
            context_switches: Some(ContextSwitches {
                total: self.context_switches.to_string(),
                per_second: rate(
                    self.context_switches,
                    prior.map(|prior| prior.context_switches),
                    elapsed_ns,
                ),
            }),
            networks: self
                .networks
                .iter()
                .map(|(name, counters)| {
                    let old = prior.and_then(|prior| prior.networks.get(name));
                    Network {
                        name: name.clone(),
                        rx_bytes: counters.rx.to_string(),
                        tx_bytes: counters.tx.to_string(),
                        rx_bytes_per_second: rate(counters.rx, old.map(|old| old.rx), elapsed_ns),
                        tx_bytes_per_second: rate(counters.tx, old.map(|old| old.tx), elapsed_ns),
                    }
                })
                .collect(),
            web_process: Some(self.web_process.clone()),
        }
    }
}

// 累计值保持u64并以字符串输出；只有已作精确相减的速率显示转换为浮点。
#[allow(clippy::cast_precision_loss)]
fn rate(value: u64, previous: Option<u64>, elapsed_ns: Option<u64>) -> Option<f64> {
    let delta = value.checked_sub(previous?)?;
    let elapsed_ns = elapsed_ns?;
    if elapsed_ns == 0 {
        return None;
    }
    Some(delta as f64 / Duration::from_nanos(elapsed_ns).as_secs_f64())
}

// CPU累计与差量先按u64校验；浮点仅表达页面百分比，不用于精确计数。
#[allow(clippy::cast_precision_loss)]
fn cpu_usage(current: &CpuCounters, previous: &CpuCounters) -> Option<f64> {
    if current.logical_cores != previous.logical_cores {
        return None;
    }
    let mut deltas = [0; 8];
    for ((delta, current), previous) in deltas.iter_mut().zip(current.ticks).zip(previous.ticks) {
        *delta = current.checked_sub(previous)?;
    }
    let total = deltas
        .iter()
        .try_fold(0_u64, |sum, value| sum.checked_add(*value))?;
    let idle = deltas[3].checked_add(deltas[4])?;
    if total == 0 {
        return None;
    }
    Some((total - idle) as f64 * 100.0 / total as f64)
}

fn integer(value: Option<&str>, label: &str) -> Result<u64, String> {
    value
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| format!("资源字段缺失或无效：{label}"))
}

fn fields<'a>(body: &'a str, key: &str) -> Result<Vec<&'a str>, String> {
    let mut found = body.lines().filter_map(|line| {
        let (name, value) = line.split_once(':')?;
        (name == key).then_some(value)
    });
    let value = found.next().ok_or_else(|| format!("资源字段缺失：{key}"))?;
    if found.next().is_some() {
        return Err(format!("资源字段重复：{key}"));
    }
    Ok(value.split_whitespace().collect())
}

fn kib(body: &str, key: &str) -> Result<u64, String> {
    let value = fields(body, key)?;
    if value.len() != 2 || value[1] != "kB" {
        return Err(format!("资源字段单位无效：{key}"));
    }
    integer(value.first().copied(), key)?
        .checked_mul(1024)
        .ok_or_else(|| format!("资源字段溢出：{key}"))
}

fn parse_stat(stat: &str) -> Result<(CpuCounters, u64), String> {
    let mut cpu = None;
    let mut cores = std::collections::BTreeSet::new();
    let mut context_switches = None;
    for line in stat.lines() {
        let mut values = line.split_whitespace();
        match values.next() {
            Some("cpu") => {
                if cpu.is_some() {
                    return Err("CPU累计行重复".into());
                }
                let mut ticks = [0; 8];
                for tick in &mut ticks {
                    *tick = integer(values.next(), "CPU累计")?;
                }
                cpu = Some(ticks);
            }
            Some("ctxt") => {
                if context_switches.is_some() {
                    return Err("整机上下文切换字段重复".into());
                }
                context_switches = Some(integer(values.next(), "ctxt")?);
            }
            Some(name)
                if name.strip_prefix("cpu").is_some_and(|id| {
                    !id.is_empty() && id.bytes().all(|value| value.is_ascii_digit())
                }) =>
            {
                for _ in 0..8 {
                    integer(values.next(), "CPU核累计")?;
                }
                if !cores.insert(name) {
                    return Err("CPU核编号重复".into());
                }
            }
            _ => {}
        }
    }
    let logical_cores = u32::try_from(cores.len()).map_err(|_| "CPU核数溢出")?;
    if logical_cores == 0 {
        return Err("缺少在线CPU核".into());
    }
    Ok((
        CpuCounters {
            ticks: cpu.ok_or("缺少CPU累计行")?,
            logical_cores,
        },
        context_switches.ok_or("缺少整机ctxt")?,
    ))
}

fn parse_memory(memory: &str) -> Result<Memory, String> {
    let total_bytes = kib(memory, "MemTotal")?;
    let available_bytes = kib(memory, "MemAvailable")?;
    let swap_total_bytes = kib(memory, "SwapTotal")?;
    let swap_free = kib(memory, "SwapFree")?;
    if total_bytes == 0 {
        return Err("整机内存总量为零".into());
    }
    Ok(Memory {
        total_bytes,
        available_bytes,
        used_bytes: total_bytes
            .checked_sub(available_bytes)
            .ok_or("可用内存超过总量")?,
        swap_total_bytes,
        swap_used_bytes: swap_total_bytes
            .checked_sub(swap_free)
            .ok_or("可用swap超过总量")?,
    })
}

fn parse_load(load: &str) -> Result<([f64; 3], Tasks), String> {
    let load: Vec<_> = load.split_whitespace().collect();
    if load.len() != 5 {
        return Err("loadavg字段数量错误".into());
    }
    let mut average = [0.0; 3];
    for (value, raw) in average.iter_mut().zip(&load) {
        *value = raw.parse::<f64>().map_err(|_| "loadavg数值无效")?;
        if !value.is_finite() || *value < 0.0 {
            return Err("loadavg非有限或为负".into());
        }
    }
    let (running, total) = load[3].split_once('/').ok_or("loadavg任务字段无效")?;
    let running = integer(Some(running), "运行中任务")?;
    let total = integer(Some(total), "总任务")?;
    if running > total || total == 0 {
        return Err("运行中任务超过总量或总量为零".into());
    }
    Ok((average, Tasks { total, running }))
}

fn parse_networks(network: &str) -> Result<BTreeMap<String, NetworkCounters>, String> {
    let mut networks = BTreeMap::new();
    let mut lines = network.lines();
    if !lines
        .next()
        .is_some_and(|line| line.contains("Inter-") && line.contains("Receive"))
        || !lines
            .next()
            .is_some_and(|line| line.contains("face") && line.contains("bytes"))
    {
        return Err("网络计数表头无效".into());
    }
    for line in lines.filter(|line| !line.trim().is_empty()) {
        let (name, values) = line.rsplit_once(':').ok_or("网络接口字段无效")?;
        let name = name.trim();
        let values: Vec<_> = values.split_whitespace().collect();
        if name.is_empty() || name.len() > 64 || values.len() != 16 {
            return Err("网络接口名称、计数列或数量无效".into());
        }
        let mut counters = [0; 16];
        for (counter, value) in counters.iter_mut().zip(values) {
            *counter = integer(Some(value), "网络计数")?;
        }
        if name != "lo" {
            if networks.len() >= 64 {
                return Err("非loopback网络接口超过64个上限".into());
            }
            if networks
                .insert(
                    name.to_owned(),
                    NetworkCounters {
                        rx: counters[0],
                        tx: counters[8],
                    },
                )
                .is_some()
            {
                return Err("网络接口重复".into());
            }
        }
    }
    Ok(networks)
}

fn parse_process(process: &str) -> Result<WebProcess, String> {
    let threads = fields(process, "Threads")?;
    if threads.len() != 1 {
        return Err("Web进程线程字段无效".into());
    }
    let threads = integer(threads.first().copied(), "Web线程数")?;
    if threads == 0 {
        return Err("Web线程数为零".into());
    }
    Ok(WebProcess {
        threads,
        resident_bytes: kib(process, "VmRSS")?,
    })
}

fn parse(
    stat: &str,
    memory: &str,
    load: &str,
    network: &str,
    process: &str,
) -> Result<RawSample, String> {
    let (cpu, context_switches) = parse_stat(stat)?;
    let (load, tasks) = parse_load(load)?;
    Ok(RawSample {
        cpu,
        load,
        memory: parse_memory(memory)?,
        tasks,
        context_switches,
        networks: parse_networks(network)?,
        web_process: parse_process(process)?,
    })
}

fn read_proc(root: &Path) -> Result<RawSample, String> {
    fn read(root: &Path, name: &str) -> Result<String, String> {
        let mut body = String::new();
        std::fs::File::open(root.join(name))
            .and_then(|file| file.take(MAX_PROC_BYTES + 1).read_to_string(&mut body))
            .map_err(|error| format!("读取/proc/{name}失败：{error}"))?;
        if body.len() as u64 > MAX_PROC_BYTES {
            return Err(format!("/proc/{name}超过256KiB读取上限"));
        }
        Ok(body)
    }
    parse(
        &read(root, "stat")?,
        &read(root, "meminfo")?,
        &read(root, "loadavg")?,
        &read(root, "net/dev")?,
        &read(root, "self/status")?,
    )
}

fn read_system() -> Result<RawSample, String> {
    if cfg!(target_os = "linux") {
        read_proc(Path::new("/proc"))
    } else {
        Err("资源采集仅支持Linux procfs".into())
    }
}

pub(super) fn router(state: ResourcesState) -> Router {
    Router::new()
        .route(
            "/api/resources",
            axum::routing::get(|State(state): State<ResourcesState>| async move {
                Json(state.snapshot())
            }),
        )
        .with_state(state)
}

#[cfg(test)]
#[path = "status_resources_tests.rs"]
mod tests;
