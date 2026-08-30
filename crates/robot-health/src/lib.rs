//! # `robot-health`
//!
//! 系统资源监控：系统级（CPU / memory / load）+ per-process（interested
//! processes + 全系统 top N by CPU）。
//!
//! ## 设计原则
//!
//! - **非 RT**：采样周期秒级（默认 5 s），普通优先级线程，允许 heap alloc /
//!   文件 IO。不得挤占 `s0_rt_thread` 资源（用 `SCHED_OTHER` 默认即可）。
//! - **Linux only**：Production 目标平台 Linux `x86_64` / `aarch64`；macOS 提供
//!   stub 返回全 0 方便 dev 机编译。Windows 不支持（不是目标平台）。
//! - **纯 `/proc` 解析**：不依赖 `sysinfo` / `procfs` crate（减少依赖面）。
//!   手写 parser + 全单测覆盖。
//! - **输出走 tracing**（`docs/logging.md` 词汇表）：`info!(cpu_usage_pct,
//!   mem_used_pct, load_1m, interested, top_by_cpu, "system health")`。
//! - **监控 ≠ supervisor**：本 crate 只**观察 + 报告**，**不**启动 / 重启
//!   服务（§6.7 "systemd 是最外层看门狗"）。supervisor / 重启策略见 task-31
//!   B'c 评估结果（`docs/CHANGELOG.md` 2026-04-25 task-31 B'a）。
//!
//! ## CPU 使用率算法
//!
//! ### 系统级
//!
//! `/proc/stat` 第一行 `cpu  user nice system idle iowait irq softirq steal
//! guest guest_nice`，单位 jiffies (`USER_HZ`=100 Hz)。两帧差分：
//! `(busy_delta / total_delta) * 100`。
//!
//! ### 进程级
//!
//! `/proc/<pid>/stat` 字段 14/15 (`utime` / `stime`)，同样 jiffies。两帧差分：
//! `(pid_cpu_delta / total_cpu_delta) * num_cpus * 100` —— `× num_cpus` 让结果
//! 和 `top` 一致（单核 100% 时显示 100.0；多核满载显示 `100 * num_cpus`）。
//!
//! ## 使用
//!
//! ```no_run
//! use std::time::Duration;
//!
//! let handle = robot_health::spawn_monitor(robot_health::MonitorConfig {
//!     interval: Duration::from_secs(5),
//!     interested_processes: vec!["s0_daemon".into(), "mock_s1".into()],
//!     top_n: 5,
//! });
//! // 运行期间 tracing::info! 会每 5 s 打一条 system health
//! handle.stop();
//! ```

#![deny(unused_must_use)]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

pub mod proc;
// v0.5 候选 E 段(8)（Spec PD-E6(c)）：sd_notify + liveness-gated watchdog
// 单一实现，自 robot-s0 迁入；S0 / S1 / gateway 三 daemon 共用。
pub mod systemd;

/// 单进程健康快照（差分 CPU% + 实时 RSS）。
#[derive(Debug, Clone, PartialEq)]
pub struct ProcessSnapshot {
    pub pid: u32,
    pub comm: String,
    pub state: char,
    /// 本进程 CPU 使用率（0-100×cpus，跨 all cores 归一化 —— 和 `top` 一致：
    /// 单核 100% 跑满 → 100.0；双核同跑 → 200.0）。
    pub cpu_pct: f32,
    pub rss_kib: u64,
}

/// 系统级健康快照（差分计算得到）。
///
/// 所有 `*_pct` 字段都是**差分结果**（对比上一帧），单帧占位 0.0。
#[derive(Debug, Clone, PartialEq)]
pub struct SystemSnapshot {
    /// 采样时刻，monotonic ns（`Instant::now()` 对 `epoch_mono` 的 offset）。
    pub sampled_mono_ns: u64,
    /// 系统总 CPU 使用率（0-100，所有 core 合并）。
    pub cpu_usage_pct: f32,
    /// 1 / 5 / 15 分钟 load average。
    pub load_1m: f32,
    pub load_5m: f32,
    pub load_15m: f32,
    /// 内存总量（KiB）。
    pub mem_total_kib: u64,
    /// 内存可用（KiB）。`MemAvailable`（内核算，比 `MemFree` 准）。
    pub mem_available_kib: u64,
    /// 使用率（已用 / 总，0-100）。
    pub mem_used_pct: f32,
    /// 用户关心的命名进程（`MonitorConfig::interested_processes` 匹配到的 `comm`）。
    /// 每个匹配 pid 一个条目，按 `(comm, pid)` 字典序稳定排序便于 diff。
    pub interested: Vec<ProcessSnapshot>,
    /// 全系统 CPU 占用 top N（`MonitorConfig::top_n`，默认 5）；按 `cpu_pct`
    /// 降序；数量不足则短。
    pub top_by_cpu: Vec<ProcessSnapshot>,
}

/// Monitor 配置。
#[derive(Debug, Clone)]
pub struct MonitorConfig {
    pub interval: Duration,
    /// 用户关心的进程名列表（精确匹配 `/proc/<pid>/stat` 的 `comm` 字段）。
    /// 典型：`["s0_daemon", "mock_s1", "robotctl", "robot-rpc", "robot-viz"]`。
    /// 空 = 不采 interested。
    pub interested_processes: Vec<String>,
    /// 全系统 CPU top N 保留多少（默认 5；0 = 不采 top）。扫全 `/proc/<pid>/stat`
    /// 成本 O(n)，n ≈ 150-500 进程，非 RT 采样可接受。
    pub top_n: usize,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(5),
            interested_processes: Vec::new(),
            top_n: 5,
        }
    }
}

/// Monitor 句柄：持 join handle + stop flag。`drop` 自动 stop + join。
pub struct MonitorHandle {
    stop: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl MonitorHandle {
    /// 请求采样线程停止并 join。幂等。
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.join.take() {
            let _ = h.join();
        }
    }
}

impl Drop for MonitorHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.join.take() {
            let _ = h.join();
        }
    }
}

/// 启动系统健康采样线程。每 `cfg.interval` 打一条 `tracing::info!`。
///
/// **非 RT** 线程，默认优先级；首帧的 `*_pct` = 0（差分需两帧），后续每帧差分。
#[must_use = "MonitorHandle 不保留会立刻 stop，用 `let _h = ...` 持住生命周期"]
pub fn spawn_monitor(cfg: MonitorConfig) -> MonitorHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_worker = Arc::clone(&stop);

    let join = thread::Builder::new()
        .name("robot-health".into())
        .spawn(move || run_monitor(&cfg, &stop_worker))
        .expect("spawn robot-health thread");

    MonitorHandle {
        stop,
        join: Some(join),
    }
}

// ============================================================================
// 采样主循环
// ============================================================================

/// 上一帧的单进程计数，差分算 CPU%。
#[derive(Clone)]
struct PidPrev {
    cpu_jiffies_total: u64,
}

fn run_monitor(cfg: &MonitorConfig, stop: &Arc<AtomicBool>) {
    let start = Instant::now();
    let mut last_cpu: Option<proc::CpuTimes> = None;
    let mut last_pid_cpu: HashMap<u32, PidPrev> = HashMap::new();
    // 首帧无法算进程 CPU%（没上一帧），仍然能输出 RSS / state，CPU 占位 0
    let num_cpus = num_cpus_online();

    while !stop.load(Ordering::Acquire) {
        let now_cpu = proc::read_cpu_times();
        let mem = proc::read_meminfo();
        let load = proc::read_loadavg();

        // 系统级 CPU 差分
        let cpu_usage_pct = match (&last_cpu, &now_cpu) {
            (Some(prev), Ok(now)) => proc::cpu_usage_delta(prev, now),
            _ => 0.0,
        };
        // 总 CPU jiffies delta（per-process 归一化用）
        let total_cpu_delta = match (&last_cpu, &now_cpu) {
            (Some(prev), Ok(now)) => now.total().saturating_sub(prev.total()),
            _ => 0,
        };
        if let Ok(ref t) = now_cpu {
            last_cpu = Some(*t);
        }

        // ---- per-process 扫描 ----
        // 非 interested 也要收集，用于 top_by_cpu 排序；扫描一次写一个 Vec。
        // 每 pid 读两个文件（stat + status）；500 进程 × 2 文件 ≈ 1000 次 read，
        // 每次 < 100 µs → 全扫描 ~100 ms 内，非 RT 可接受。
        let all: Vec<ProcessSnapshot> =
            collect_process_snapshots(total_cpu_delta, num_cpus, &mut last_pid_cpu);

        // interested：精确 comm 匹配
        let mut interested: Vec<ProcessSnapshot> = all
            .iter()
            .filter(|p| cfg.interested_processes.iter().any(|n| n == &p.comm))
            .cloned()
            .collect();
        interested.sort_by(|a, b| a.comm.cmp(&b.comm).then(a.pid.cmp(&b.pid)));

        // top_by_cpu：cpu_pct 降序，取前 N
        let top_by_cpu: Vec<ProcessSnapshot> = if cfg.top_n > 0 {
            let mut sorted = all;
            sorted.sort_by(|a, b| {
                b.cpu_pct
                    .partial_cmp(&a.cpu_pct)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            sorted.truncate(cfg.top_n);
            sorted
        } else {
            Vec::new()
        };

        let snapshot = SystemSnapshot {
            sampled_mono_ns: u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX),
            cpu_usage_pct,
            load_1m: load.as_ref().map_or(0.0, |l| l.load_1m),
            load_5m: load.as_ref().map_or(0.0, |l| l.load_5m),
            load_15m: load.as_ref().map_or(0.0, |l| l.load_15m),
            mem_total_kib: mem.as_ref().map_or(0, |m| m.total_kib),
            mem_available_kib: mem.as_ref().map_or(0, |m| m.available_kib),
            mem_used_pct: mem.as_ref().map_or(0.0, proc::MemInfo::used_pct),
            interested,
            top_by_cpu,
        };

        emit_tracing(&snapshot);

        // check-then-sleep 粗粒度 stop 响应（最坏 interval 延时才退出）
        let deadline = Instant::now() + cfg.interval;
        while Instant::now() < deadline && !stop.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(100));
        }
    }
}

/// 扫 `/proc`，对每个 pid 读 stat + status，算 CPU% + RSS。
///
/// `last_pid_cpu` 保存上一帧的 `cpu_jiffies_total` 做 delta；本帧结束后会**只保留**
/// 本帧存活的 `pid`（防止长期运行 `HashMap` 无限增长 —— 短命进程 pid 会被 GC）。
fn collect_process_snapshots(
    total_cpu_delta: u64,
    num_cpus: u32,
    last_pid_cpu: &mut HashMap<u32, PidPrev>,
) -> Vec<ProcessSnapshot> {
    let Ok(pids) = proc::list_pids() else {
        return Vec::new();
    };

    let mut current: HashMap<u32, PidPrev> = HashMap::with_capacity(pids.len());
    let mut out = Vec::with_capacity(pids.len());

    for pid in pids {
        let Ok(stat) = proc::read_pid_stat(pid) else {
            // 进程在扫到之前已退出 / 权限不够 → 跳过
            continue;
        };
        let rss_kib = proc::read_pid_rss_kib(pid).unwrap_or(0);

        let cpu_pct = if let Some(prev) = last_pid_cpu.get(&pid) {
            per_process_cpu_pct(
                prev.cpu_jiffies_total,
                stat.cpu_jiffies_total,
                total_cpu_delta,
                num_cpus,
            )
        } else {
            0.0
        };

        current.insert(
            pid,
            PidPrev {
                cpu_jiffies_total: stat.cpu_jiffies_total,
            },
        );

        out.push(ProcessSnapshot {
            pid: stat.pid,
            comm: stat.comm,
            state: stat.state,
            cpu_pct,
            rss_kib,
        });
    }

    *last_pid_cpu = current;
    out
}

/// 算 per-process CPU% —— 结果语义与 `top` 一致：单核 100% 跑满 → 100.0。
///
/// `total_cpu_delta = 0` → 返回 0（两帧间总 CPU 没前进，理论上不会发生；
/// 实测极短 interval 可能 0）。
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn per_process_cpu_pct(
    prev_jiffies: u64,
    now_jiffies: u64,
    total_cpu_delta: u64,
    num_cpus: u32,
) -> f32 {
    if total_cpu_delta == 0 {
        return 0.0;
    }
    let delta = now_jiffies.saturating_sub(prev_jiffies);
    (delta as f32 / total_cpu_delta as f32) * num_cpus as f32 * 100.0
}

/// 读取在线 CPU 数量。
///
/// 来源 `/sys/devices/system/cpu/online`（如 `0-7`）；失败 fallback `1`。
fn num_cpus_online() -> u32 {
    std::fs::read_to_string("/sys/devices/system/cpu/online")
        .ok()
        .and_then(|s| parse_cpu_online(&s))
        .unwrap_or(1)
}

/// 解析 `/sys/.../online` 格式 `"0-7"` 或 `"0,2-5,8"`；返回 CPU 总数。
fn parse_cpu_online(text: &str) -> Option<u32> {
    let mut count = 0_u32;
    for part in text.trim().split(',') {
        if let Some((lo, hi)) = part.split_once('-') {
            let lo: u32 = lo.parse().ok()?;
            let hi: u32 = hi.parse().ok()?;
            count += hi.saturating_sub(lo) + 1;
        } else if let Ok(_n) = part.parse::<u32>() {
            count += 1;
        }
    }
    if count == 0 { None } else { Some(count) }
}

/// 把 snapshot 打到 tracing —— 结构化字段按 `docs/logging.md` 词汇表。
///
/// per-process 数据量较大，`interested` / `top_by_cpu` 作为 Debug String 字段
/// 打（一行内呈现，JSON 模式下是数组 embedded string）。后续如果需要单独
/// 字段可扩 —— 先保证"一行看全貌"。
fn emit_tracing(s: &SystemSnapshot) {
    let interested_dbg = summarize(&s.interested);
    let top_dbg = summarize(&s.top_by_cpu);
    tracing::info!(
        cpu_usage_pct = s.cpu_usage_pct,
        load_1m = s.load_1m,
        load_5m = s.load_5m,
        load_15m = s.load_15m,
        mem_total_kib = s.mem_total_kib,
        mem_available_kib = s.mem_available_kib,
        mem_used_pct = s.mem_used_pct,
        interested = %interested_dbg,
        top_by_cpu = %top_dbg,
        "system health"
    );
}

/// 把 Vec<ProcessSnapshot> 压成一行 String。
///
/// 格式 `pid:comm@cpu%[rss_MiB]`，多条用 `,` 分。空 Vec → `"-"` 方便肉眼识别。
fn summarize(ps: &[ProcessSnapshot]) -> String {
    if ps.is_empty() {
        return "-".into();
    }
    let mut out = String::with_capacity(ps.len() * 32);
    for (i, p) in ps.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        // rss KiB → MiB 整数显示
        let rss_mib = p.rss_kib / 1024;
        // 用 std::fmt::Write 错误忽略（String 写入不会失败）
        let _ = std::fmt::write(
            &mut out,
            format_args!(
                "{pid}:{comm}@{cpu:.1}%[{rss}M]",
                pid = p.pid,
                comm = p.comm,
                cpu = p.cpu_pct,
                rss = rss_mib
            ),
        );
    }
    out
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_fields_equal_self() {
        let s = SystemSnapshot {
            sampled_mono_ns: 1,
            cpu_usage_pct: 10.0,
            load_1m: 0.5,
            load_5m: 0.3,
            load_15m: 0.2,
            mem_total_kib: 1_000_000,
            mem_available_kib: 500_000,
            mem_used_pct: 50.0,
            interested: Vec::new(),
            top_by_cpu: Vec::new(),
        };
        assert_eq!(s, s.clone());
    }

    #[test]
    fn default_interval_5s_top_n_5() {
        let cfg = MonitorConfig::default();
        assert_eq!(cfg.interval, Duration::from_secs(5));
        assert_eq!(cfg.top_n, 5);
        assert!(
            cfg.interested_processes.is_empty(),
            "expected empty, got {:?}",
            cfg.interested_processes
        );
    }

    #[test]
    fn per_process_cpu_pct_zero_total() {
        assert!(per_process_cpu_pct(10, 20, 0, 4).abs() < 1e-6);
    }

    #[test]
    fn per_process_cpu_pct_single_core_100pct() {
        // delta=100 jiffies, total=100 jiffies, 1 cpu → 100%
        let pct = per_process_cpu_pct(0, 100, 100, 1);
        assert!((pct - 100.0).abs() < 1e-3);
    }

    #[test]
    fn per_process_cpu_pct_single_core_half() {
        // delta=50, total=100, 1 cpu → 50%
        let pct = per_process_cpu_pct(0, 50, 100, 1);
        assert!((pct - 50.0).abs() < 1e-3);
    }

    #[test]
    fn per_process_cpu_pct_multi_core_scaling() {
        // 4 cpu 全满时 delta=400, total=400（每 cpu 100 jiffies）→ 400%
        let pct = per_process_cpu_pct(0, 400, 400, 4);
        assert!((pct - 400.0).abs() < 1e-3);
    }

    #[test]
    fn parse_cpu_online_simple_range() {
        assert_eq!(parse_cpu_online("0-7\n"), Some(8));
    }

    #[test]
    fn parse_cpu_online_single_cpu() {
        assert_eq!(parse_cpu_online("0"), Some(1));
    }

    #[test]
    fn parse_cpu_online_mixed() {
        // "0,2-5,8" = 1 + 4 + 1 = 6
        assert_eq!(parse_cpu_online("0,2-5,8"), Some(6));
    }

    #[test]
    fn parse_cpu_online_empty() {
        assert_eq!(parse_cpu_online(""), None);
    }

    #[test]
    fn summarize_empty() {
        assert_eq!(summarize(&[]), "-");
    }

    #[test]
    fn summarize_one() {
        let s = summarize(&[ProcessSnapshot {
            pid: 123,
            comm: "s0_daemon".into(),
            state: 'R',
            cpu_pct: 15.5,
            rss_kib: 51200, // 50 MiB
        }]);
        assert_eq!(s, "123:s0_daemon@15.5%[50M]");
    }

    #[test]
    fn summarize_multiple() {
        let s = summarize(&[
            ProcessSnapshot {
                pid: 1,
                comm: "a".into(),
                state: 'R',
                cpu_pct: 1.0,
                rss_kib: 1024,
            },
            ProcessSnapshot {
                pid: 2,
                comm: "b".into(),
                state: 'S',
                cpu_pct: 2.0,
                rss_kib: 2048,
            },
        ]);
        assert_eq!(s, "1:a@1.0%[1M],2:b@2.0%[2M]");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn spawn_monitor_returns_handle_and_stops() {
        let h = spawn_monitor(MonitorConfig {
            interval: Duration::from_millis(100),
            interested_processes: Vec::new(),
            top_n: 3,
        });
        std::thread::sleep(Duration::from_millis(250));
        h.stop();
    }
}
