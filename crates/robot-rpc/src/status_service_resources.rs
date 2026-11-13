//! 在统一采样线程内只读统计robot服务cgroup；内存按顶层计费，不叠加进程RSS。

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;

const SCOPE: &str = "system.slice/robot-*.service";
const MAX_SERVICES: usize = 32;
const MAX_PROCESSES: usize = 256;
const MAX_THREADS: usize = 2048;
const MAX_DIRECTORIES: usize = 1024;
const MAX_ENTRIES: usize = 32_768;
const MAX_FILES: usize = 8192;
const MAX_FILE_BYTES: u64 = 65_536;
const MAX_TOTAL_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Default, Serialize)]
pub(super) struct Metrics {
    cpu_cores: Option<f64>,
    cpu_percent: Option<f64>,
    memory_bytes: Option<u64>,
    shmem_bytes: Option<u64>,
    processes: Option<u64>,
    threads: Option<u64>,
    context_switches_per_second: Option<f64>,
}

#[derive(Clone, Serialize)]
struct ServiceView {
    name: String,
    label: String,
    description: String,
    status: &'static str,
    error: Option<String>,
    #[serde(flatten)]
    metrics: Metrics,
}

#[derive(Clone, Serialize)]
pub(super) struct Snapshot {
    status: &'static str,
    error: Option<String>,
    scope: &'static str,
    service_count: usize,
    totals: Metrics,
    services: Vec<ServiceView>,
}

impl Snapshot {
    pub(super) fn unavailable(error: impl Into<String>) -> Self {
        Self {
            status: "unavailable",
            error: Some(bounded(&error.into())),
            scope: SCOPE,
            service_count: 0,
            totals: Metrics::default(),
            services: Vec::new(),
        }
    }
}

fn bounded(value: &str) -> String {
    let mut result = String::new();
    for ch in value.chars() {
        let ch = if ch.is_control() { ' ' } else { ch };
        if result.len() + ch.len_utf8() > 240 {
            break;
        }
        result.push(ch);
    }
    result
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Identity {
    device: u64,
    inode: u64,
}

#[cfg(unix)]
fn identity(path: &Path) -> Result<Identity, String> {
    use std::os::unix::fs::MetadataExt;
    let value =
        std::fs::symlink_metadata(path).map_err(|error| format!("读取目录身份失败：{error}"))?;
    if !value.is_dir() {
        return Err("cgroup或task路径不是普通目录".into());
    }
    Ok(Identity {
        device: value.dev(),
        inode: value.ino(),
    })
}

#[cfg(not(unix))]
fn identity(_path: &Path) -> Result<Identity, String> {
    Err("cgroup资源采集仅支持Linux".into())
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ThreadKey {
    pid: u32,
    tid: u32,
    start_time: u64,
}

#[derive(Clone)]
struct RawService {
    identity: Identity,
    cpu_usec: u64,
    memory: u64,
    shmem: u64,
    threads: u64,
    pids: BTreeSet<u32>,
    switches: Option<BTreeMap<ThreadKey, u64>>,
    context_error: Option<String>,
}

#[derive(Clone)]
pub(super) struct RawSnapshot {
    entries: Result<BTreeMap<String, Result<RawService, String>>, String>,
}

impl RawSnapshot {
    pub(super) fn unavailable(error: impl Into<String>) -> Self {
        Self {
            entries: Err(bounded(&error.into())),
        }
    }

    pub(super) fn view(
        &self,
        stamp_ns: u64,
        previous: Option<(u64, &Self)>,
        logical_cores: u32,
    ) -> Snapshot {
        let entries = match &self.entries {
            Ok(entries) => entries,
            Err(error) => return Snapshot::unavailable(error.clone()),
        };
        let elapsed = previous
            .and_then(|(stamp, _)| stamp_ns.checked_sub(stamp))
            .filter(|elapsed| *elapsed > 0);
        let prior = previous.and_then(|(_, snapshot)| snapshot.entries.as_ref().ok());
        let services: Vec<_> = entries
            .iter()
            .map(|(name, raw)| {
                let (label, description) = describe(name);
                let mut view = ServiceView {
                    name: name.clone(),
                    label,
                    description,
                    status: "unavailable",
                    error: None,
                    metrics: Metrics::default(),
                };
                match raw {
                    Err(error) => view.error = Some(bounded(error)),
                    Ok(raw) => {
                        let old = prior
                            .and_then(|prior| prior.get(name))
                            .and_then(|old| old.as_ref().ok())
                            .filter(|old| old.identity == raw.identity);
                        let cpu_cores = old
                            .and_then(|old| raw.cpu_usec.checked_sub(old.cpu_usec))
                            .and_then(|delta| elapsed.map(|elapsed| cpu_rate(delta, elapsed)));
                        let context = old
                            .and_then(|old| context_delta(raw, old))
                            .and_then(|delta| elapsed.map(|elapsed| count_rate(delta, elapsed)));
                        view.status = if raw.pids.is_empty() {
                            "inactive"
                        } else {
                            "running"
                        };
                        view.error.clone_from(&raw.context_error);
                        view.metrics = Metrics {
                            cpu_cores,
                            cpu_percent: cpu_cores
                                .filter(|_| logical_cores > 0)
                                .map(|cores| cores * 100.0 / f64::from(logical_cores)),
                            memory_bytes: Some(raw.memory),
                            shmem_bytes: Some(raw.shmem),
                            processes: u64::try_from(raw.pids.len()).ok(),
                            threads: Some(raw.threads),
                            context_switches_per_second: context,
                        };
                    }
                }
                view
            })
            .collect();
        let partial = services.iter().any(|service| service.error.is_some());
        Snapshot {
            status: if partial { "partial" } else { "ok" },
            error: partial.then(|| "部分服务资源无法完整读取，缺失字段不计部分合计".into()),
            scope: SCOPE,
            service_count: services.len(),
            totals: totals(&services),
            services,
        }
    }
}

// 累计微秒与切换计数先作u64差分；浮点仅用于页面速率显示。
#[allow(clippy::cast_precision_loss)]
fn count_rate(delta: u64, elapsed_ns: u64) -> f64 {
    delta as f64 / Duration::from_nanos(elapsed_ns).as_secs_f64()
}

fn cpu_rate(delta_usec: u64, elapsed_ns: u64) -> f64 {
    Duration::from_micros(delta_usec).as_secs_f64() / Duration::from_nanos(elapsed_ns).as_secs_f64()
}

fn context_delta(current: &RawService, previous: &RawService) -> Option<u64> {
    if current.pids != previous.pids {
        return None;
    }
    let current = current.switches.as_ref()?;
    let previous = previous.switches.as_ref()?;
    if current.len() != previous.len() || !current.keys().eq(previous.keys()) {
        return None;
    }
    current.iter().try_fold(0_u64, |sum, (key, value)| {
        sum.checked_add(value.checked_sub(*previous.get(key)?)?)
    })
}

fn totals(services: &[ServiceView]) -> Metrics {
    fn integer(services: &[ServiceView], get: impl Fn(&Metrics) -> Option<u64>) -> Option<u64> {
        services.iter().try_fold(0_u64, |sum, service| {
            sum.checked_add(get(&service.metrics)?)
        })
    }
    fn number(services: &[ServiceView], get: impl Fn(&Metrics) -> Option<f64>) -> Option<f64> {
        services.iter().try_fold(0.0, |sum, service| {
            let sum = sum + get(&service.metrics)?;
            sum.is_finite().then_some(sum)
        })
    }
    Metrics {
        cpu_cores: number(services, |metrics| metrics.cpu_cores),
        cpu_percent: number(services, |metrics| metrics.cpu_percent),
        memory_bytes: integer(services, |metrics| metrics.memory_bytes),
        shmem_bytes: integer(services, |metrics| metrics.shmem_bytes),
        processes: integer(services, |metrics| metrics.processes),
        threads: integer(services, |metrics| metrics.threads),
        context_switches_per_second: number(services, |metrics| {
            metrics.context_switches_per_second
        }),
    }
}

fn describe(name: &str) -> (String, String) {
    let (label, description) = match name {
        "robot-s0-bus1.service" => ("左侧S0", "左臂、头部硬件运行时，含本组相机采集辅助进程"),
        "robot-s0-bus2.service" => ("右侧S0", "右臂、底盘硬件运行时，含本组相机采集辅助进程"),
        "robot-gateway-bus1.service" => ("左侧网关", "转发左侧机器人状态与控制RPC"),
        "robot-gateway-bus2.service" => ("右侧网关", "转发右侧机器人状态与控制RPC"),
        "robot-status.service" => ("Web状态服务", "只读状态、相机预览与统一资源采样"),
        _ if name.starts_with("robot-s1") => ("S1任务服务", "上层任务与既有动作后端，含本组子进程"),
        _ => (
            "机器人服务",
            "按robot服务cgroup统计，包含嵌套子组与辅助进程",
        ),
    };
    (label.into(), description.into())
}

#[derive(Default)]
struct Budget {
    directories: usize,
    entries: usize,
    files: usize,
    bytes: usize,
    processes: usize,
    threads: usize,
}

impl Budget {
    fn read(&mut self, path: &Path) -> Result<String, String> {
        self.files += 1;
        if self.files > MAX_FILES {
            return Err("资源采样超过8192次文件读取上限".into());
        }
        let mut body = String::new();
        std::fs::File::open(path)
            .and_then(|file| file.take(MAX_FILE_BYTES + 1).read_to_string(&mut body))
            .map_err(|error| format!("读取{}失败：{error}", path.display()))?;
        self.bytes = self
            .bytes
            .checked_add(body.len())
            .ok_or("资源读取字节数溢出")?;
        if body.len() as u64 > MAX_FILE_BYTES || self.bytes > MAX_TOTAL_BYTES {
            return Err("资源采样超过单文件64KiB或总计16MiB上限".into());
        }
        Ok(body)
    }

    fn list(&mut self, path: &Path) -> Result<Vec<(String, bool)>, String> {
        self.directories += 1;
        if self.directories > MAX_DIRECTORIES {
            return Err("资源采样超过1024次目录读取上限".into());
        }
        let mut result = Vec::new();
        for entry in std::fs::read_dir(path)
            .map_err(|error| format!("枚举{}失败：{error}", path.display()))?
        {
            self.entries += 1;
            if self.entries > MAX_ENTRIES {
                return Err("资源采样超过32768个目录项上限".into());
            }
            let entry = entry.map_err(|error| error.to_string())?;
            let kind = entry.file_type().map_err(|error| error.to_string())?;
            if kind.is_symlink() {
                return Err("资源采样拒绝目录项符号链接".into());
            }
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| "目录项名称不是UTF-8")?;
            result.push((name, kind.is_dir()));
        }
        result.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(result)
    }
}

fn unsigned(value: &str) -> Result<u64, String> {
    value
        .trim()
        .parse()
        .map_err(|_| "资源计数不是有效u64".into())
}

fn key(body: &str, key: &str) -> Result<u64, String> {
    let mut found = None;
    for line in body.lines() {
        let mut fields = line.split_whitespace();
        if fields.next() != Some(key) {
            continue;
        }
        if found.is_some() {
            return Err(format!("字段重复：{key}"));
        }
        let value = fields.next().ok_or_else(|| format!("字段缺值：{key}"))?;
        if fields.next().is_some() {
            return Err(format!("字段列数错误：{key}"));
        }
        found = Some(unsigned(value)?);
    }
    found.ok_or_else(|| format!("字段缺失：{key}"))
}

#[derive(PartialEq, Eq)]
struct Tree {
    directories: BTreeMap<PathBuf, Identity>,
    pids: BTreeSet<u32>,
}

fn cgroup_tree(root: &Path, budget: &mut Budget) -> Result<Tree, String> {
    let mut directories = BTreeMap::new();
    let mut pids = BTreeSet::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        if directories.len() >= 128 {
            return Err("单服务嵌套cgroup超过128个上限".into());
        }
        directories.insert(path.clone(), identity(&path)?);
        for value in budget.read(&path.join("cgroup.procs"))?.split_whitespace() {
            let pid = u32::try_from(unsigned(value)?).map_err(|_| "PID溢出")?;
            if pid == 0 {
                return Err("PID不能为零".into());
            }
            pids.insert(pid);
            if pids.len() > MAX_PROCESSES {
                return Err("单服务PID超过256个上限".into());
            }
        }
        for (name, is_dir) in budget.list(&path)? {
            if is_dir {
                pending.push(path.join(name));
            }
        }
    }
    Ok(Tree { directories, pids })
}

fn thread_ids(proc: &Path, pid: u32, budget: &mut Budget) -> Result<BTreeSet<u32>, String> {
    budget
        .list(&proc.join(pid.to_string()).join("task"))?
        .into_iter()
        .map(|(name, directory)| {
            if !directory {
                return Err("task成员不是目录".into());
            }
            let tid = u32::try_from(unsigned(&name)?).map_err(|_| "TID溢出")?;
            if tid == 0 {
                return Err("TID不能为零".into());
            }
            Ok(tid)
        })
        .collect()
}

fn start_time(body: &str, tid: u32) -> Result<u64, String> {
    let (id, tail) = body.split_once(" (").ok_or("线程stat缺少名称")?;
    if unsigned(id)? != u64::from(tid) {
        return Err("线程stat身份不匹配".into());
    }
    let (_, fields) = tail.rsplit_once(") ").ok_or("线程stat名称不完整")?;
    unsigned(
        fields
            .split_whitespace()
            .nth(19)
            .ok_or("线程stat缺少starttime")?,
    )
}

fn thread_switches(
    proc: &Path,
    pid: u32,
    tid: u32,
    budget: &mut Budget,
) -> Result<(ThreadKey, u64), String> {
    let path = proc
        .join(pid.to_string())
        .join("task")
        .join(tid.to_string());
    let before = start_time(&budget.read(&path.join("stat"))?, tid)?;
    let status = budget.read(&path.join("status"))?;
    if key(&status, "Pid:")? != u64::from(tid) || key(&status, "Tgid:")? != u64::from(pid) {
        return Err("线程status身份不匹配".into());
    }
    let total = key(&status, "voluntary_ctxt_switches:")?
        .checked_add(key(&status, "nonvoluntary_ctxt_switches:")?)
        .ok_or("线程切换计数溢出")?;
    if before != start_time(&budget.read(&path.join("stat"))?, tid)? {
        return Err("读取期间线程身份改变".into());
    }
    Ok((
        ThreadKey {
            pid,
            tid,
            start_time: before,
        },
        total,
    ))
}

fn switches(
    proc: &Path,
    pids: &BTreeSet<u32>,
    budget: &mut Budget,
) -> Result<BTreeMap<ThreadKey, u64>, String> {
    let mut result = BTreeMap::new();
    for &pid in pids {
        let before = thread_ids(proc, pid, budget)?;
        budget.threads = budget
            .threads
            .checked_add(before.len())
            .ok_or("线程数量溢出")?;
        if before.is_empty() || budget.threads > MAX_THREADS {
            return Err("线程集合为空或全轮线程超过2048个上限".into());
        }
        for &tid in &before {
            let (key, count) = thread_switches(proc, pid, tid, budget)?;
            result.insert(key, count);
        }
        if before != thread_ids(proc, pid, budget)? {
            return Err("读取期间线程成员变化".into());
        }
    }
    Ok(result)
}

fn read_service(path: &Path, proc: &Path, budget: &mut Budget) -> Result<RawService, String> {
    let before = cgroup_tree(path, budget)?;
    budget.processes = budget
        .processes
        .checked_add(before.pids.len())
        .ok_or("进程数量溢出")?;
    if budget.processes > MAX_PROCESSES {
        return Err("全轮PID超过256个上限".into());
    }
    let cpu_usec = key(&budget.read(&path.join("cpu.stat"))?, "usage_usec")?;
    let memory = unsigned(&budget.read(&path.join("memory.current"))?)?;
    let shmem = key(&budget.read(&path.join("memory.stat"))?, "shmem")?;
    let threads = unsigned(&budget.read(&path.join("pids.current"))?)?;
    let (switches, context_error) = match switches(proc, &before.pids, budget) {
        Ok(values) if u64::try_from(values.len()).ok() == Some(threads) => (Some(values), None),
        Ok(_) => (None, Some("线程成员数与pids.current不一致".into())),
        Err(error) => (None, Some(bounded(&format!("线程切换未采集：{error}")))),
    };
    if before != cgroup_tree(path, budget)?
        || threads != unsigned(&budget.read(&path.join("pids.current"))?)?
    {
        return Err("读取期间cgroup身份、PID或线程数变化".into());
    }
    Ok(RawService {
        identity: *before.directories.get(path).ok_or("缺少顶层cgroup身份")?,
        cpu_usec,
        memory,
        shmem,
        threads,
        pids: before.pids,
        switches,
        context_error,
    })
}

fn service_directories(
    root: &Path,
    budget: &mut Budget,
) -> Result<BTreeMap<String, Identity>, String> {
    let mut result = BTreeMap::new();
    for (name, directory) in budget.list(root)? {
        if directory && name.starts_with("robot-") && name.ends_with(".service") {
            if name.len() > 128
                || name.chars().any(char::is_control)
                || result.len() >= MAX_SERVICES
            {
                return Err("robot服务名称超长或服务数超过32个上限".into());
            }
            result.insert(name.clone(), identity(&root.join(name))?);
        }
    }
    Ok(result)
}

pub(super) fn read(root: &Path, proc: &Path) -> RawSnapshot {
    let result: Result<_, String> = (|| {
        let mut budget = Budget::default();
        let parent = identity(root)?;
        let before = service_directories(root, &mut budget)?;
        let mut entries = BTreeMap::new();
        for name in before.keys() {
            entries.insert(
                name.clone(),
                read_service(&root.join(name), proc, &mut budget).map_err(|error| bounded(&error)),
            );
        }
        if identity(root)? != parent || service_directories(root, &mut budget)? != before {
            return Err("读取期间robot服务目录集合或身份变化".into());
        }
        let mut owners = BTreeMap::new();
        let mut duplicates = BTreeSet::new();
        for (name, service) in &entries {
            if let Ok(service) = service {
                for pid in &service.pids {
                    if let Some(old) = owners.insert(*pid, name.clone()) {
                        duplicates.insert(old);
                        duplicates.insert(name.clone());
                    }
                }
            }
        }
        for name in duplicates {
            entries.insert(name, Err("采样期间PID跨服务重复，拒绝重复计数".into()));
        }
        Ok(entries)
    })();
    RawSnapshot {
        entries: result.map_err(|error| bounded(&error)),
    }
}

#[cfg(test)]
#[path = "status_service_resources_tests.rs"]
mod tests;
