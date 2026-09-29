//! 自有 worker 生命周期监督。SDK 内部 Drop 可能等待网络，最终收尾由进程边界负责。

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use robot_core::time::{Deadline, DurationNs, RobotClock, SystemClock};

use crate::invalid;

static STOP: AtomicBool = AtomicBool::new(false);
const PROGRESS: &[u8] = b"ROBOT_VIZ_PROGRESS ";
const QUIET_TIMEOUT_NS: u64 = 15_000_000_000;
const STOP_TIMEOUT_NS: u64 = 10_000_000_000;

struct Worker {
    child: Child,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        // 所有早退都必须回收自己创建的 worker；已 wait 的 Child 会返回缓存状态。
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

extern "C" fn signal_handler(_signal: i32) {
    STOP.store(true, Ordering::Relaxed);
}

/// 信号处理器仅修改无锁原子位，不访问 SDK、堆或文件。
pub fn install_signals() {
    // SAFETY：静态 C ABI 函数生命周期覆盖进程，只做信号安全的原子 store。
    unsafe {
        let handler = signal_handler as *const () as libc::sighandler_t;
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    }
}

#[must_use]
pub fn stopping() -> bool {
    STOP.load(Ordering::Relaxed)
}

/// 私有 worker 标准输出只传短进度记录；父进程持续读取，媒体不经过管道。
pub fn progress(stage: &str) -> std::io::Result<()> {
    let mut out = std::io::stdout().lock();
    writeln!(out, "ROBOT_VIZ_PROGRESS {stage}")?;
    out.flush()
}

pub fn run(
    args: &[std::ffi::OsString],
    report: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let clock = SystemClock;
    let child = Command::new(std::env::current_exe()?)
        .args(args)
        .env("ROBOT_VIZ_INTERNAL_WORKER", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    let mut worker = Worker {
        child,
        reader: None,
    };
    let mut stdout = worker
        .child
        .stdout
        .take()
        .ok_or_else(|| invalid("worker stdout 管道缺失"))?;
    let heartbeat = Arc::new(AtomicU64::new(clock.now().0));
    let stopping_at = Arc::new(AtomicU64::new(0));
    let reader_heartbeat = Arc::clone(&heartbeat);
    let reader_stopping = Arc::clone(&stopping_at);
    let reader =
        std::thread::spawn(move || read_progress(&mut stdout, &reader_heartbeat, &reader_stopping));
    worker.reader = Some(reader);
    let mut signal_sent = false;
    let mut forced_failure = None;
    let status = loop {
        if let Some(status) = worker.child.try_wait()? {
            break status;
        }
        let now = clock.now().0;
        if stopping() && !signal_sent {
            signal_sent = true;
            stopping_at.store(now, Ordering::Release);
            let pid = i32::try_from(worker.child.id())?;
            // SAFETY：仅向仍由本父进程持有、尚未回收的子进程发送停止信号。
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
        }
        let stop = stopping_at.load(Ordering::Acquire);
        let quiet = now.saturating_sub(heartbeat.load(Ordering::Acquire)) >= QUIET_TIMEOUT_NS;
        let shutdown_expired = stop != 0 && now.saturating_sub(stop) >= STOP_TIMEOUT_NS;
        if quiet || shutdown_expired {
            forced_failure = Some(if shutdown_expired {
                "worker 收尾超过 10 秒，已终止并回收"
            } else {
                "worker 超过 15 秒无进展，已终止并回收"
            });
            let kill_result = worker.child.kill();
            let status = worker.child.wait()?;
            kill_result?;
            break status;
        }
        clock.sleep_until(Deadline::from_now_plus(
            clock.now(),
            DurationNs::from_millis(20),
        ));
    };
    if let Some(reader) = worker.reader.take() {
        reader
            .join()
            .map_err(|_| invalid("worker 管道读取任务 panic"))?;
    }
    if let Some(reason) = forced_failure {
        write_failure(report, reason)?;
        return Err(invalid(reason).into());
    }
    if !status.success() {
        let reason = format!("观测 worker 失败：{status}");
        write_failure(report, &reason)?;
        return Err(invalid(reason).into());
    }
    if let Some(path) = report {
        let mut value: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)?;
        value["worker_reaped"] = serde_json::Value::Bool(true);
        std::fs::write(path, serde_json::to_vec_pretty(&value)?)?;
    }
    Ok(())
}

fn write_failure(path: Option<&Path>, reason: &str) -> Result<(), Box<dyn std::error::Error>> {
    let Some(path) = path else {
        return Ok(());
    };
    let mut value: serde_json::Value = std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    value["output_status"] = serde_json::Value::String("error".into());
    value["supervisor_error"] = serde_json::Value::String(reason.into());
    value["worker_reaped"] = serde_json::Value::Bool(true);
    std::fs::write(path, serde_json::to_vec_pretty(&value)?)?;
    Ok(())
}

fn read_progress(
    stdout: &mut impl Read,
    reader_heartbeat: &AtomicU64,
    reader_stopping: &AtomicU64,
) {
    let mut buffer = [0u8; 256];
    let mut line = Vec::with_capacity(256);
    while let Ok(count) = stdout.read(&mut buffer) {
        if count == 0 {
            break;
        }
        for byte in &buffer[..count] {
            if *byte == b'\n' {
                if line.starts_with(PROGRESS) {
                    let now = SystemClock.now().0;
                    reader_heartbeat.store(now, Ordering::Release);
                    if line[PROGRESS.len()..] == *b"stopping" {
                        let _ = reader_stopping.compare_exchange(
                            0,
                            now,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        );
                    }
                }
                line.clear();
            } else if line.len() < 4096 {
                line.push(*byte);
            }
        }
    }
}
