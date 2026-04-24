//! # `robot-health`
//!
//! 系统资源监控。当前只提供系统级（`/proc/stat` 总 CPU + `/proc/meminfo` 内存
//! + `/proc/loadavg` load），后续扩 per-process（RSS / CPU%）+ 全系统 top5。
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
//!   mem_used_pct, load_1m, "system health")`，外部 journald / Loki 能直接
//!   结构化采集。后续可加 iceoryx2 topic `/<robot_id>/s0/system_health`
//!   给其他进程订阅（不在本骨架）。
//!
//! ## CPU 使用率算法
//!
//! `/proc/stat` 第一行 `cpu  user nice system idle iowait irq softirq steal
//! guest guest_nice`，单位 jiffies (`USER_HZ`=100 Hz)。使用率是**差分**：两次
//! 采样做差，`(total_delta - idle_delta) / total_delta * 100`。**单次采样
//! 无法计算**，必须持有上一帧 snapshot 做 delta。
//!
//! ## 使用
//!
//! ```no_run
//! use std::time::Duration;
//!
//! let handle = robot_health::spawn_monitor(
//!     robot_health::MonitorConfig {
//!         interval: Duration::from_secs(5),
//!     },
//! );
//! // 运行期间 tracing::info! 会每 5 s 打一条 system health
//! handle.stop();
//! ```

#![deny(unused_must_use)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

pub mod proc;

/// 系统级健康快照（差分计算得到）。
///
/// 所有字段都是**差分结果**（对比上一帧），单帧无意义。`cpu_usage_pct`
/// 为 `f32`（-1 表 "还没第一次 delta"，实测不会出现，单测边界）。
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
}

/// Monitor 配置。
#[derive(Debug, Clone, Copy)]
pub struct MonitorConfig {
    pub interval: Duration,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(5),
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
        // 保险：如果调用方没 stop 就 drop，这里也停掉
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.join.take() {
            let _ = h.join();
        }
    }
}

/// 启动系统健康采样线程。每 `cfg.interval` 打一条 `tracing::info!`。
///
/// **非 RT** 线程，默认优先级；首次采样立刻返回（`cpu_usage_pct=0` —— 需要
/// 两次采样做差才有意义），后续每帧差分。
#[must_use = "MonitorHandle 不保留会立刻 stop，用 `let _h = ...` 持住生命周期"]
pub fn spawn_monitor(cfg: MonitorConfig) -> MonitorHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_worker = Arc::clone(&stop);

    let join = thread::Builder::new()
        .name("robot-health".into())
        .spawn(move || run_monitor(cfg, &stop_worker))
        .expect("spawn robot-health thread");

    MonitorHandle {
        stop,
        join: Some(join),
    }
}

fn run_monitor(cfg: MonitorConfig, stop: &Arc<AtomicBool>) {
    let start = Instant::now();
    let mut last_cpu: Option<proc::CpuTimes> = None;

    while !stop.load(Ordering::Acquire) {
        let now_cpu = proc::read_cpu_times();
        let mem = proc::read_meminfo();
        let load = proc::read_loadavg();

        let cpu_usage_pct = match (&last_cpu, &now_cpu) {
            (Some(prev), Ok(now)) => proc::cpu_usage_delta(prev, now),
            _ => 0.0,
        };
        if let Ok(ref t) = now_cpu {
            last_cpu = Some(*t);
        }

        let snapshot = SystemSnapshot {
            sampled_mono_ns: u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX),
            cpu_usage_pct,
            load_1m: load.as_ref().map_or(0.0, |l| l.load_1m),
            load_5m: load.as_ref().map_or(0.0, |l| l.load_5m),
            load_15m: load.as_ref().map_or(0.0, |l| l.load_15m),
            mem_total_kib: mem.as_ref().map_or(0, |m| m.total_kib),
            mem_available_kib: mem.as_ref().map_or(0, |m| m.available_kib),
            mem_used_pct: mem.as_ref().map_or(0.0, proc::MemInfo::used_pct),
        };

        tracing::info!(
            cpu_usage_pct = snapshot.cpu_usage_pct,
            load_1m = snapshot.load_1m,
            load_5m = snapshot.load_5m,
            load_15m = snapshot.load_15m,
            mem_total_kib = snapshot.mem_total_kib,
            mem_available_kib = snapshot.mem_available_kib,
            mem_used_pct = snapshot.mem_used_pct,
            "system health"
        );

        // 用 check-then-sleep 粗粒度 stop 响应（最坏 interval 延时才退出）。
        // 秒级采样可接受。更急的场景可拆成 100ms 轮询 + check。
        let deadline = Instant::now() + cfg.interval;
        while Instant::now() < deadline && !stop.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(100));
        }
    }
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
        };
        assert_eq!(s, s.clone());
    }

    #[test]
    fn default_interval_5s() {
        assert_eq!(MonitorConfig::default().interval, Duration::from_secs(5));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn spawn_monitor_returns_handle_and_stops() {
        let h = spawn_monitor(MonitorConfig {
            interval: Duration::from_millis(100),
        });
        std::thread::sleep(Duration::from_millis(250));
        h.stop();
    }
}
