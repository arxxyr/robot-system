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
}
