//! Linux `/proc` 解析（CPU / memory / loadavg）。
//!
//! 纯整数 / 字符串解析，零外部 crate 依赖。macOS stub 返回 Err 让上层 fallback
//! 到 0（dev 机跑编译，生产走 Linux）。

use std::fs;
use std::io;

// ============================================================================
// CPU /proc/stat
// ============================================================================

/// `/proc/stat` 第一行总 CPU 时间（单位 jiffies，`USER_HZ`=100 Hz）。
///
/// 字段顺序（内核文档 `man proc`）：
/// `user nice system idle iowait irq softirq steal guest guest_nice`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuTimes {
    pub user: u64,
    pub nice: u64,
    pub system: u64,
    pub idle: u64,
    pub iowait: u64,
    pub irq: u64,
    pub softirq: u64,
    pub steal: u64,
}

impl CpuTimes {
    /// 累计 "busy" 时间（user + nice + system + irq + softirq + steal）。
    #[must_use]
    pub const fn busy(&self) -> u64 {
        self.user + self.nice + self.system + self.irq + self.softirq + self.steal
    }

    /// 累计 "idle" 时间（idle + iowait）。
    #[must_use]
    pub const fn idle_total(&self) -> u64 {
        self.idle + self.iowait
    }

    /// 累计总时间。
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.busy() + self.idle_total()
    }
}

/// 读 `/proc/stat`（Linux 专用）。
///
/// # Errors
/// - 文件不存在（非 Linux） → `io::Error`
/// - 格式不对（第一行不是 `cpu ` 开头 / 字段少） → `io::ErrorKind::InvalidData`
pub fn read_cpu_times() -> io::Result<CpuTimes> {
    let text = fs::read_to_string("/proc/stat")?;
    parse_cpu_times(&text)
}

/// 解析 `/proc/stat` 文本。分离出来便于单测喂 fixture。
pub fn parse_cpu_times(text: &str) -> io::Result<CpuTimes> {
    let line = text
        .lines()
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "empty /proc/stat"))?;
    let mut parts = line.split_whitespace();
    let tag = parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no tag in /proc/stat line"))?;
    if tag != "cpu" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("/proc/stat first line tag '{tag}' != 'cpu'"),
        ));
    }
    let mut vals: [u64; 8] = [0; 8];
    for slot in &mut vals {
        let s = parts.next().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "/proc/stat cpu line short of 8 fields",
            )
        })?;
        *slot = s.parse::<u64>().map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("/proc/stat parse u64 '{s}': {e}"),
            )
        })?;
    }
    Ok(CpuTimes {
        user: vals[0],
        nice: vals[1],
        system: vals[2],
        idle: vals[3],
        iowait: vals[4],
        irq: vals[5],
        softirq: vals[6],
        steal: vals[7],
    })
}

/// 两帧 CPU times 差分算使用率（0-100）。
///
/// `total_delta == 0`（两次采样完全没变，极短 interval + 无活动）→ 返回 0。
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn cpu_usage_delta(prev: &CpuTimes, now: &CpuTimes) -> f32 {
    let total_delta = now.total().saturating_sub(prev.total());
    let idle_delta = now.idle_total().saturating_sub(prev.idle_total());
    if total_delta == 0 {
        return 0.0;
    }
    let busy_delta = total_delta.saturating_sub(idle_delta);
    (busy_delta as f32 / total_delta as f32) * 100.0
}

// ============================================================================
// Memory /proc/meminfo
// ============================================================================

/// 内存快照（KiB）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemInfo {
    pub total_kib: u64,
    pub available_kib: u64,
    pub free_kib: u64,
}

impl MemInfo {
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn used_pct(&self) -> f32 {
        if self.total_kib == 0 {
            return 0.0;
        }
        let used = self.total_kib.saturating_sub(self.available_kib);
        (used as f32 / self.total_kib as f32) * 100.0
    }
}

/// 读 `/proc/meminfo`。
///
/// # Errors
/// 文件不存在 / 格式异常。
pub fn read_meminfo() -> io::Result<MemInfo> {
    let text = fs::read_to_string("/proc/meminfo")?;
    parse_meminfo(&text)
}

/// 解析 `/proc/meminfo` 文本。支持缺失字段（返回 0 fallback）。
pub fn parse_meminfo(text: &str) -> io::Result<MemInfo> {
    let mut total_kib = 0_u64;
    let mut available_kib = 0_u64;
    let mut free_kib = 0_u64;
    for line in text.lines() {
        if let Some(v) = parse_meminfo_line(line, "MemTotal:") {
            total_kib = v;
        } else if let Some(v) = parse_meminfo_line(line, "MemAvailable:") {
            available_kib = v;
        } else if let Some(v) = parse_meminfo_line(line, "MemFree:") {
            free_kib = v;
        }
    }
    if total_kib == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "MemTotal missing or zero in /proc/meminfo",
        ));
    }
    // MemAvailable 在很老的内核（< 3.14）没有，fallback 到 MemFree（偏保守）
    if available_kib == 0 {
        available_kib = free_kib;
    }
    Ok(MemInfo {
        total_kib,
        available_kib,
        free_kib,
    })
}

/// 解析形如 `Name:        12345 kB` 的行。
fn parse_meminfo_line(line: &str, prefix: &str) -> Option<u64> {
    let rest = line.strip_prefix(prefix)?;
    let value_str = rest.split_whitespace().next()?;
    value_str.parse::<u64>().ok()
}

// ============================================================================
// Load /proc/loadavg
// ============================================================================

/// 1 / 5 / 15 分钟 load average。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LoadAvg {
    pub load_1m: f32,
    pub load_5m: f32,
    pub load_15m: f32,
}

/// 读 `/proc/loadavg`。
///
/// # Errors
/// 文件不存在 / 格式异常。
pub fn read_loadavg() -> io::Result<LoadAvg> {
    let text = fs::read_to_string("/proc/loadavg")?;
    parse_loadavg(&text)
}

/// 解析 `/proc/loadavg` 文本（`<1m> <5m> <15m> <running/total> <last_pid>`）。
pub fn parse_loadavg(text: &str) -> io::Result<LoadAvg> {
    let mut parts = text.split_whitespace();
    let one_min = next_f32(&mut parts, "load_1m")?;
    let five_min = next_f32(&mut parts, "load_5m")?;
    let fifteen_min = next_f32(&mut parts, "load_15m")?;
    Ok(LoadAvg {
        load_1m: one_min,
        load_5m: five_min,
        load_15m: fifteen_min,
    })
}

fn next_f32<'a>(it: &mut impl Iterator<Item = &'a str>, name: &str) -> io::Result<f32> {
    let s = it
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("missing {name}")))?;
    s.parse::<f32>().map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("parse {name} '{s}': {e}"),
        )
    })
}

// ============================================================================
// Per-process /proc/<pid>/stat + /proc/<pid>/status
// ============================================================================

/// 单进程原始计数（未做 delta）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PidStatRaw {
    pub pid: u32,
    /// 进程名（`/proc/<pid>/stat` 的 `comm` 字段，原始括号已剥掉；可能含空格）。
    pub comm: String,
    /// `state` 字段（`R` Running / `S` Sleeping / `D` Uninterruptible / `Z` Zombie
    /// / `T` Stopped / `X` Dead 等，见 `man proc`）。
    pub state: char,
    /// `utime + stime`（jiffies）—— user + system 的累计 CPU 时间。delta 算
    /// per-process CPU% 用这一列。
    pub cpu_jiffies_total: u64,
}

/// 解析 `/proc/<pid>/stat` 一行。
///
/// **坑**：`comm` 字段被括号包围，**可能含空格 + 括号**（Python 解释器 `(python
/// 3)` 这种）。正确做法：找**最后一个 `)`**，把 `[0]` 当 pid、`[1..last_paren]`
/// 当 comm、`last_paren` 之后的字段按空格切。
///
/// # Errors
/// 格式异常返回 `InvalidData`。
pub fn parse_pid_stat(text: &str) -> io::Result<PidStatRaw> {
    let line = text
        .lines()
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "empty /proc/<pid>/stat"))?;
    let last_paren = line.rfind(')').ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "no closing paren in /proc/<pid>/stat line",
        )
    })?;
    let first_paren = line.find('(').ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "no opening paren in /proc/<pid>/stat line",
        )
    })?;
    if first_paren >= last_paren {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "malformed parens in /proc/<pid>/stat",
        ));
    }
    // pid（第一个空格之前）
    let pid_end = line[..first_paren].trim_end();
    let pid: u32 = pid_end.trim().parse().map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("pid '{pid_end}' parse: {e}"),
        )
    })?;
    // comm（括号之间）
    let comm = line[first_paren + 1..last_paren].to_string();
    // 剩余字段（last_paren+1 之后，按空格切；index 0 = state）
    let rest = line[last_paren + 1..].trim();
    let mut fields = rest.split_whitespace();
    let state_str = fields
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing state field"))?;
    let state = state_str
        .chars()
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "empty state field"))?;
    // man proc 字段索引从 3 起（tgid / ppid / pgrp / session / tty_nr / tpgid
    // / flags / minflt / cminflt / majflt / cmajflt / utime / stime）
    // → 在 rest 里，state 是 index 0，utime 是 index 11，stime 是 index 12
    let remaining: Vec<&str> = fields.collect();
    if remaining.len() < 12 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("expected ≥ 12 fields after state, got {}", remaining.len()),
        ));
    }
    let utime: u64 = remaining[10].parse().map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("utime '{}' parse: {e}", remaining[10]),
        )
    })?;
    let stime: u64 = remaining[11].parse().map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("stime '{}' parse: {e}", remaining[11]),
        )
    })?;

    Ok(PidStatRaw {
        pid,
        comm,
        state,
        cpu_jiffies_total: utime + stime,
    })
}

/// 读 `/proc/<pid>/stat`。
///
/// # Errors
/// 进程不存在 / 无权限 → `io::Error`。
pub fn read_pid_stat(pid: u32) -> io::Result<PidStatRaw> {
    let text = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    parse_pid_stat(&text)
}

/// 读 `/proc/<pid>/status` 里的 `VmRSS`（KiB）。比 `/proc/<pid>/stat` 第 24
/// 字段好用：直接 KiB 单位，不用 × `page_size`。
///
/// # Errors
/// 进程不存在 / 无权限 / 无 `VmRSS`（kernel thread 无虚存）。
pub fn read_pid_rss_kib(pid: u32) -> io::Result<u64> {
    let text = fs::read_to_string(format!("/proc/{pid}/status"))?;
    parse_vm_rss_kib(&text)
}

/// 解析 `/proc/<pid>/status` 里 `VmRSS:  12345 kB` 行。
pub fn parse_vm_rss_kib(text: &str) -> io::Result<u64> {
    for line in text.lines() {
        if let Some(v) = parse_meminfo_line(line, "VmRSS:") {
            return Ok(v);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "VmRSS line not present (kernel thread?)",
    ))
}

/// 扫 `/proc`，收集所有数字目录名（= pid）。
///
/// # Errors
/// `/proc` 不可读 → `io::Error`。
pub fn list_pids() -> io::Result<Vec<u32>> {
    let mut out = Vec::with_capacity(256);
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        if let Some(name) = entry.file_name().to_str()
            && let Ok(pid) = name.parse::<u32>()
        {
            out.push(pid);
        }
    }
    Ok(out)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    const PROC_STAT_SAMPLE: &str = "\
cpu  123456 789 45678 987654 1234 567 890 123 0 0
cpu0 61728 394 22839 493827 617 283 445 61 0 0
cpu1 61728 395 22839 493827 617 284 445 62 0 0
intr 12345678 0 0 0
ctxt 987654321
btime 1700000000
processes 123456
";

    #[test]
    fn parse_cpu_times_ok() {
        let t = parse_cpu_times(PROC_STAT_SAMPLE).unwrap();
        assert_eq!(t.user, 123_456);
        assert_eq!(t.nice, 789);
        assert_eq!(t.system, 45_678);
        assert_eq!(t.idle, 987_654);
        assert_eq!(t.iowait, 1234);
        assert_eq!(t.irq, 567);
        assert_eq!(t.softirq, 890);
        assert_eq!(t.steal, 123);
    }

    #[test]
    fn parse_cpu_times_empty_rejected() {
        assert!(parse_cpu_times("").is_err());
    }

    #[test]
    fn parse_cpu_times_wrong_tag_rejected() {
        let err = parse_cpu_times("mem  1 2 3 4 5 6 7 8").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn parse_cpu_times_short_line_rejected() {
        let err = parse_cpu_times("cpu  1 2 3").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn cpu_times_busy_total_idle() {
        let t = CpuTimes {
            user: 10,
            nice: 2,
            system: 5,
            idle: 80,
            iowait: 1,
            irq: 0,
            softirq: 0,
            steal: 0,
        };
        assert_eq!(t.busy(), 17);
        assert_eq!(t.idle_total(), 81);
        assert_eq!(t.total(), 98);
    }

    #[test]
    fn cpu_usage_delta_zero_when_no_change() {
        let t = CpuTimes {
            user: 10,
            nice: 0,
            system: 0,
            idle: 100,
            iowait: 0,
            irq: 0,
            softirq: 0,
            steal: 0,
        };
        // 两帧完全一致，delta=0 → 使用率 0
        assert!(cpu_usage_delta(&t, &t).abs() < 1e-6);
    }

    #[test]
    fn cpu_usage_delta_50_percent() {
        let prev = CpuTimes {
            user: 0,
            nice: 0,
            system: 0,
            idle: 100,
            iowait: 0,
            irq: 0,
            softirq: 0,
            steal: 0,
        };
        let now = CpuTimes {
            user: 100,
            nice: 0,
            system: 0,
            idle: 200, // 多 100 idle，多 100 user → 各 50%
            iowait: 0,
            irq: 0,
            softirq: 0,
            steal: 0,
        };
        let pct = cpu_usage_delta(&prev, &now);
        assert!((pct - 50.0).abs() < 0.01, "expected ~50%, got {pct}");
    }

    #[test]
    fn cpu_usage_delta_100_percent() {
        let prev = CpuTimes {
            user: 0,
            nice: 0,
            system: 0,
            idle: 100,
            iowait: 0,
            irq: 0,
            softirq: 0,
            steal: 0,
        };
        let now = CpuTimes {
            user: 200,
            nice: 0,
            system: 0,
            idle: 100, // 全走 user,0 idle 增量
            iowait: 0,
            irq: 0,
            softirq: 0,
            steal: 0,
        };
        let pct = cpu_usage_delta(&prev, &now);
        assert!((pct - 100.0).abs() < 0.01, "expected ~100%, got {pct}");
    }

    // ---- meminfo ----

    const PROC_MEMINFO_SAMPLE: &str = "\
MemTotal:       16333668 kB
MemFree:         2345678 kB
MemAvailable:    8765432 kB
Buffers:          456789 kB
Cached:          3456789 kB
";

    #[test]
    fn parse_meminfo_ok() {
        let m = parse_meminfo(PROC_MEMINFO_SAMPLE).unwrap();
        assert_eq!(m.total_kib, 16_333_668);
        assert_eq!(m.free_kib, 2_345_678);
        assert_eq!(m.available_kib, 8_765_432);
    }

    #[test]
    fn parse_meminfo_used_pct() {
        let m = MemInfo {
            total_kib: 10_000,
            available_kib: 2_500,
            free_kib: 2_000,
        };
        // used = 10000-2500 = 7500, pct = 75%
        assert!((m.used_pct() - 75.0).abs() < 0.01);
    }

    #[test]
    fn parse_meminfo_fallback_to_free_when_available_missing() {
        let text = "\
MemTotal:       1000 kB
MemFree:         400 kB
";
        let m = parse_meminfo(text).unwrap();
        assert_eq!(m.available_kib, 400); // fallback to MemFree
    }

    #[test]
    fn parse_meminfo_missing_total_rejected() {
        let err = parse_meminfo("MemFree: 100 kB\n").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn parse_meminfo_used_pct_zero_total() {
        let m = MemInfo {
            total_kib: 0,
            available_kib: 0,
            free_kib: 0,
        };
        assert!(m.used_pct().abs() < 1e-6);
    }

    // ---- loadavg ----

    #[test]
    fn parse_loadavg_ok() {
        // 典型 /proc/loadavg：`0.12 0.34 0.56 2/345 67890\n`
        let l = parse_loadavg("0.12 0.34 0.56 2/345 67890\n").unwrap();
        assert!((l.load_1m - 0.12).abs() < 0.001);
        assert!((l.load_5m - 0.34).abs() < 0.001);
        assert!((l.load_15m - 0.56).abs() < 0.001);
    }

    #[test]
    fn parse_loadavg_short_rejected() {
        assert!(parse_loadavg("0.12 0.34").is_err());
    }

    // ---- 非 Linux stub（Err）----

    #[test]
    #[cfg(not(target_os = "linux"))]
    fn non_linux_read_cpu_errs() {
        // 其他 OS 没 /proc，调 read_cpu_times 必然 Err
        assert!(read_cpu_times().is_err());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn linux_live_read_cpu_works() {
        // 真机 Linux 能读出 /proc/stat
        let t = read_cpu_times().unwrap();
        assert!(t.total() > 0);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn linux_live_read_meminfo_works() {
        let m = read_meminfo().unwrap();
        assert!(m.total_kib > 0);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn linux_live_read_loadavg_works() {
        let l = read_loadavg().unwrap();
        assert!(l.load_1m >= 0.0);
    }

    // ---- pid stat parsing ----

    /// 真实 `/proc/<pid>/stat` sample（comm 无空格）。
    /// 字段: pid=1 comm=(systemd) state=S ... utime=42 stime=99 ...
    const PID_STAT_SIMPLE: &str = "\
1 (systemd) S 0 1 1 0 -1 4194560 100 200 0 0 42 99 0 0 20 0 1 0 12 34 56 78";

    /// comm 含空格 + 嵌入括号（类似 `(python 3)` / `(kworker u:1)`）。
    const PID_STAT_TRICKY_COMM: &str = "\
1234 (kworker u:1-events) S 2 1234 0 0 -1 0 0 0 0 0 100 200 0 0 20 0 1 0 12 34 56 78";

    #[test]
    fn parse_pid_stat_simple() {
        let r = parse_pid_stat(PID_STAT_SIMPLE).unwrap();
        assert_eq!(r.pid, 1);
        assert_eq!(r.comm, "systemd");
        assert_eq!(r.state, 'S');
        // utime=42, stime=99 → total=141
        assert_eq!(r.cpu_jiffies_total, 141);
    }

    #[test]
    fn parse_pid_stat_tricky_comm_with_spaces_and_parens() {
        let r = parse_pid_stat(PID_STAT_TRICKY_COMM).unwrap();
        assert_eq!(r.pid, 1234);
        assert_eq!(r.comm, "kworker u:1-events"); // 空格保留
        assert_eq!(r.state, 'S');
        assert_eq!(r.cpu_jiffies_total, 300);
    }

    #[test]
    fn parse_pid_stat_no_parens_rejected() {
        assert!(parse_pid_stat("1 systemd S 0 1").is_err());
    }

    #[test]
    fn parse_pid_stat_empty_rejected() {
        assert!(parse_pid_stat("").is_err());
    }

    #[test]
    fn parse_pid_stat_short_fields_rejected() {
        // 只有 state 之后 5 个字段，少于 12 → 拒绝
        assert!(parse_pid_stat("1 (a) S 0 1 1 0 -1").is_err());
    }

    // ---- VmRSS ----

    const PID_STATUS_SAMPLE: &str = "\
Name:   systemd
State:  S (sleeping)
Tgid:   1
Pid:    1
VmPeak: 100000 kB
VmSize: 98765 kB
VmLck:  0 kB
VmPin:  0 kB
VmHWM:  45678 kB
VmRSS:  43210 kB
RssAnon:    20000 kB
";

    #[test]
    fn parse_vm_rss_kib_ok() {
        assert_eq!(parse_vm_rss_kib(PID_STATUS_SAMPLE).unwrap(), 43_210);
    }

    #[test]
    fn parse_vm_rss_kib_missing_rejected() {
        // kernel thread 无 VmRSS
        let text = "Name: kthreadd\nState: S\n";
        let err = parse_vm_rss_kib(text).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    // ---- Live Linux ----

    #[test]
    #[cfg(target_os = "linux")]
    fn linux_live_list_pids_contains_self() {
        let pids = list_pids().unwrap();
        let me = std::process::id();
        assert!(
            pids.contains(&me),
            "list_pids() should contain current pid {me}, got {} entries",
            pids.len()
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn linux_live_read_self_stat() {
        let me = std::process::id();
        let r = read_pid_stat(me).unwrap();
        assert_eq!(r.pid, me);
        // 当前进程必在运行（或 R 或 S），不该是 Zombie
        assert!(
            matches!(r.state, 'R' | 'S' | 'D' | 't' | 'T'),
            "unexpected state {:?}",
            r.state
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn linux_live_read_self_rss() {
        let me = std::process::id();
        let rss = read_pid_rss_kib(me).unwrap();
        assert!(rss > 0, "self RSS should be > 0, got {rss}");
    }
}
