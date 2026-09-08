//! AC-16：真实单次串口调用停滞必须使 S0 停喂 systemd，调用恢复后重新喂狗。

use std::io;
use std::os::unix::net::UnixDatagram;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use robot_core::serial_services::BusOpClass;
use robot_core::time::{RobotClock, SystemClock};
use robot_s0::comm_fault_hook::{HookCommandKind, RESULT_SUCCESS, TestCommFaultCommand};
use robot_s0::comm_fault_observation::ObservationPage;
use serde_json::{Value, json};
use tokio::process::Child;

use super::Args;
use super::control::ControlPort;
use super::harness::{Case, stop};
use super::heartbeat::HeartbeatFeed;
use super::report::{Report, file_hash};

const READ_INDEX: usize = BusOpClass::PeriodicRead as usize;
const MAX_DATAGRAMS: usize = 2048;
const MAX_GAPS: usize = 128;
const MAX_OBSERVER_GAP_NS: u64 = 50_000_000;
const COMMAND_SEQUENCE: u64 = 1;

struct Datagram {
    received_ns: u64,
    payload: Vec<u8>,
}

impl Datagram {
    fn contains(&self, field: &[u8]) -> bool {
        self.payload
            .split(|byte| *byte == b'\n')
            .any(|line| line == field)
    }
}

#[derive(Default)]
struct ReceiverState {
    datagrams: Vec<Datagram>,
    slow_polls: Vec<(u64, u64)>,
    lost: bool,
    error: Option<String>,
}

struct Capture {
    path: PathBuf,
    state: Arc<Mutex<ReceiverState>>,
    stopping: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    removed: bool,
}

impl Capture {
    fn start(case: &Case) -> Result<Self, String> {
        // sockaddr_un 路径有长度上限；不用可能很深的 evidence 目录作 socket 地址。
        let path = PathBuf::from("/tmp").join(format!("robot-cf-wd-{}.sock", case.robot_id));
        let socket = UnixDatagram::bind(&path)
            .map_err(|error| format!("创建独立 notify socket 失败：{error}"))?;
        let mut capture = Self {
            path,
            state: Arc::new(Mutex::new(ReceiverState {
                datagrams: Vec::with_capacity(MAX_DATAGRAMS),
                slow_polls: Vec::with_capacity(MAX_GAPS),
                ..ReceiverState::default()
            })),
            stopping: Arc::new(AtomicBool::new(false)),
            worker: None,
            removed: false,
        };
        socket
            .set_read_timeout(Some(Duration::from_millis(5)))
            .map_err(|error| error.to_string())?;
        let state = capture.state.clone();
        let stopping = capture.stopping.clone();
        capture.worker = Some(
            std::thread::Builder::new()
                .name("验收 notify 收包".to_owned())
                .spawn(move || receive(&socket, &state, &stopping))
                .map_err(|error| error.to_string())?,
        );
        Ok(capture)
    }

    fn check(&self) -> Result<(), String> {
        let state = self.state.lock().unwrap();
        if state.lost {
            return Err("notify 观测器容量不足或 datagram 截断，证据无效".to_owned());
        }
        if let Some(error) = &state.error {
            return Err(format!("notify 收包失败：{error}"));
        }
        if self.worker.as_ref().is_none_or(JoinHandle::is_finished) {
            return Err("notify 观测线程已提前退出".to_owned());
        }
        Ok(())
    }

    fn count_watchdog_after(&self, after_ns: u64) -> usize {
        self.state
            .lock()
            .unwrap()
            .datagrams
            .iter()
            .filter(|message| message.received_ns >= after_ns && message.contains(b"WATCHDOG=1"))
            .count()
    }

    fn received_ready(&self) -> bool {
        self.state
            .lock()
            .unwrap()
            .datagrams
            .iter()
            .any(|message| message.contains(b"READY=1"))
    }

    fn check_quiet_window(&self, start_ns: u64, end_ns: u64) -> Result<(), String> {
        self.check()?;
        let state = self.state.lock().unwrap();
        if state
            .slow_polls
            .iter()
            .any(|(before, after)| *before <= end_ns && *after >= start_ns)
        {
            return Err("停喂窗口内 notify 观测器出现超过 50ms 的调度空档，证据无效".to_owned());
        }
        if let Some(message) = state.datagrams.iter().find(|message| {
            (start_ns..=end_ns).contains(&message.received_ns) && message.contains(b"WATCHDOG=1")
        }) {
            return Err(format!(
                "串口调用停滞期间仍收到 WATCHDOG，received_ns={}",
                message.received_ns
            ));
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<ReceiverState, String> {
        self.stopping.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.join().map_err(|_| "notify 收包线程 panic")?;
        }
        std::fs::remove_file(&self.path)
            .map_err(|error| format!("删除独立 notify socket 失败：{error}"))?;
        self.removed = true;
        Ok(std::mem::take(&mut *self.state.lock().unwrap()))
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        if !self.removed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn receive(socket: &UnixDatagram, state: &Mutex<ReceiverState>, stopping: &AtomicBool) {
    let mut buffer = [0; 4096];
    let mut previous_poll = SystemClock.now().0;
    while !stopping.load(Ordering::Acquire) {
        let result = socket.recv(&mut buffer);
        let now = SystemClock.now().0;
        let mut state = state.lock().unwrap();
        if now.saturating_sub(previous_poll) > MAX_OBSERVER_GAP_NS {
            if state.slow_polls.len() < MAX_GAPS {
                state.slow_polls.push((previous_poll, now));
            } else {
                state.lost = true;
            }
        }
        previous_poll = now;
        match result {
            Ok(length) => {
                if length == buffer.len() || state.datagrams.len() == MAX_DATAGRAMS {
                    state.lost = true;
                } else {
                    state.datagrams.push(Datagram {
                        received_ns: now,
                        payload: buffer[..length].to_vec(),
                    });
                }
            }
            Err(error) => match error.kind() {
                io::ErrorKind::WouldBlock
                | io::ErrorKind::TimedOut
                | io::ErrorKind::Interrupted => {}
                _ => {
                    state.error = Some(error.to_string());
                    break;
                }
            },
        }
    }
}

fn alive(child: &mut Child) -> Result<(), String> {
    match child.try_wait().map_err(|error| error.to_string())? {
        None => Ok(()),
        Some(status) => Err(format!("S0 在验收中提前退出：{status}")),
    }
}

#[derive(Clone, Copy)]
struct CallEntry {
    count: u64,
    observed_ns: u64,
    lower_ns: u64,
}

async fn wait_healthy(
    child: &mut Child,
    capture: &Capture,
    page: &ObservationPage,
) -> Result<(), String> {
    let deadline = SystemClock.now().0 + 5_000_000_000;
    loop {
        alive(child)?;
        capture.check()?;
        let observation = page.snapshot();
        if observation.ready_reached
            && observation.hook_ready
            && capture.received_ready()
            && capture.count_watchdog_after(0) >= 3
        {
            return Ok(());
        }
        if SystemClock.now().0 >= deadline {
            return Err("真实 READY 后未收到至少三次 WATCHDOG".to_owned());
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

async fn wait_call_entry(
    child: &mut Child,
    capture: &Capture,
    page: &ObservationPage,
    baseline: u64,
    baseline_ns: u64,
    observations: &mut Vec<Value>,
) -> Result<CallEntry, String> {
    let deadline = SystemClock.now().0 + 3_000_000_000;
    let mut last_count = baseline;
    let mut last_poll = baseline_ns;
    let mut candidate = None;
    loop {
        alive(child)?;
        capture.check()?;
        let observation = page.snapshot();
        let now = SystemClock.now().0;
        let count = observation.serial_calls[READ_INDEX];
        if count != last_count {
            observations.push(json!({"observed_ns": now, "lower_ns": last_poll,
                "periodic_read_calls": count, "command_applied": observation.command_applied}));
            candidate = Some(CallEntry {
                count,
                observed_ns: now,
                lower_ns: last_poll,
            });
            last_count = count;
        }
        // 周期读本来持续递增；应用注入后保持同一调用 75ms 才确认进入目标停滞。
        if let Some(entry) = candidate.as_ref()
            && count > baseline
            && observation.command_applied == COMMAND_SEQUENCE
            && now.saturating_sub(entry.observed_ns) >= 75_000_000
        {
            if entry.observed_ns.saturating_sub(entry.lower_ns) > MAX_OBSERVER_GAP_NS {
                return Err("串口调用入口观测区间超过 50ms，不能可靠锚定停喂窗口".to_owned());
            }
            return Ok(*entry);
        }
        if now >= deadline {
            return Err("注入后未观察到真实 PeriodicRead 调用进入并停滞".to_owned());
        }
        last_poll = now;
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

async fn check_stall(
    case: &Case,
    child: &mut Child,
    capture: &Capture,
    page: &ObservationPage,
    entry: &CallEntry,
    observations: &mut Vec<Value>,
) -> Result<(u64, u64), String> {
    let start_ns = entry.observed_ns + 300_000_000;
    let end_ns = entry.observed_ns + 1_500_000_000;
    loop {
        alive(child)?;
        capture.check()?;
        let count = page.snapshot().serial_calls[READ_INDEX];
        if count != entry.count {
            return Err("目标调用在停喂检查窗完成前已返回或发生额外 PeriodicRead".to_owned());
        }
        let now = SystemClock.now().0;
        if now >= start_ns {
            capture.check_quiet_window(start_ns, now.min(end_ns))?;
        }
        if now >= end_ns {
            observations.push(json!({"observed_ns": now, "periodic_read_calls": count, "phase": "quiet_window_end"}));
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let logs = case.logs("s0")?;
    if !logs
        .lines()
        .any(|line| line.contains("serial_io_thread") && line.contains("worker tick stalled"))
    {
        return Err("停喂窗口内缺少真实 serial_io_thread stalled 诊断".to_owned());
    }
    Ok((start_ns, end_ns))
}

async fn wait_recovery(
    child: &mut Child,
    capture: &Capture,
    page: &ObservationPage,
    entry: &CallEntry,
    observations: &mut Vec<Value>,
) -> Result<u64, String> {
    let deadline = entry.observed_ns + 4_000_000_000;
    let mut resumed = None;
    loop {
        alive(child)?;
        capture.check()?;
        let now = SystemClock.now().0;
        let count = page.snapshot().serial_calls[READ_INDEX];
        if count > entry.count && resumed.is_none() {
            resumed = Some(now);
            observations.push(
                json!({"observed_ns": now, "periodic_read_calls": count, "phase": "call_resumed"}),
            );
        }
        if let Some(resumed_ns) = resumed
            && capture.count_watchdog_after(resumed_ns) >= 3
        {
            return Ok(resumed_ns);
        }
        if now >= deadline {
            return Err("串口调用恢复后未重新收到三次真实 WATCHDOG".to_owned());
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

async fn scenario(
    case: &Case,
    child: &mut Child,
    capture: &Capture,
    observations: &mut Vec<Value>,
) -> Result<Value, String> {
    let _heartbeat = HeartbeatFeed::start(&case.robot_id)?;
    let control = ControlPort::new(&case.robot_id)?;
    let page = ObservationPage::open(&case.directory.join("s0.observation"))
        .map_err(|error| error.to_string())?;
    wait_healthy(child, capture, &page).await?;
    let before = page.snapshot();
    let baseline_ns = SystemClock.now().0;
    observations.push(json!({"observed_ns": baseline_ns, "periodic_read_calls": before.serial_calls[READ_INDEX], "phase": "healthy_baseline"}));
    let watchdog_before = capture.count_watchdog_after(0);
    control.send(TestCommFaultCommand {
        sequence: COMMAND_SEQUENCE,
        duration_ms: 2000,
        count: 1,
        kind: HookCommandKind::SerialResult as u8,
        target: BusOpClass::PeriodicRead as u8,
        error: RESULT_SUCCESS,
        reserved: [0; 5],
    })?;
    let entry = wait_call_entry(
        child,
        capture,
        &page,
        before.serial_calls[READ_INDEX],
        baseline_ns,
        observations,
    )
    .await?;
    let (start_ns, end_ns) = check_stall(case, child, capture, &page, &entry, observations).await?;
    let resumed_ns = wait_recovery(child, capture, &page, &entry, observations).await?;
    capture.check_quiet_window(start_ns, end_ns)?;
    alive(child)?;
    Ok(
        json!({"watchdog_usec": 200_000, "healthy_watchdog_count": watchdog_before,
        "injected_duration_ms": 2000, "serial_call_count": entry.count,
        "call_entry_lower_ns": entry.lower_ns, "call_entry_observed_ns": entry.observed_ns,
        "quiet_window_start_ns": start_ns, "quiet_window_end_ns": end_ns,
        "quiet_watchdog_count": 0, "call_resumed_observed_ns": resumed_ns,
        "recovered_watchdog_count": capture.count_watchdog_after(resumed_ns),
        "scope": "真实 S0、真实 WATCHDOG datagram 与实际 serial 调用入口计数；独立 socket 不连接或修改任何 systemd unit。"}),
    )
}

async fn cleanup_child(child: &mut Child) -> Result<(), String> {
    match stop(child).await {
        Ok(()) => Ok(()),
        Err(error) => {
            // 正常关停失败仍回收本场景拥有的子进程，但不能把强制终止算作 PASS。
            let force = match child.start_kill() {
                Ok(()) => tokio::time::timeout(Duration::from_secs(2), child.wait())
                    .await
                    .map_err(|_| "强制回收超时".to_owned())
                    .and_then(|status| status.map_err(|error| error.to_string())),
                Err(error) => Err(error.to_string()),
            };
            Err(format!("正常关停失败：{error}；强制回收结果={force:?}"))
        }
    }
}

async fn execute(args: &Args) -> Result<Value, String> {
    let case = Case::new(
        &args.evidence_dir,
        &args.bin_dir,
        "watchdog_single_serial_stall",
        6,
    )?;
    let mut capture = Capture::start(&case)?;
    let mut command = case.command();
    command
        .args(["--notify-mode", "required"])
        .env("NOTIFY_SOCKET", &capture.path)
        .env("WATCHDOG_USEC", "200000")
        .env_remove("WATCHDOG_PID")
        .env("RUST_LOG", "info");
    let mut child = case.spawn("s0", command)?;
    let mut observations = Vec::new();
    let result = scenario(&case, &mut child, &capture, &mut observations).await;
    let cleanup = cleanup_child(&mut child).await;
    let received = capture.finish();
    let (datagrams, gaps, receiver_error, lost) = match received {
        Ok(state) => (
            state
                .datagrams
                .into_iter()
                .map(|message| {
                    json!({
                        "received_monotonic_raw_ns": message.received_ns,
                        "payload_bytes": message.payload,
                    })
                })
                .collect::<Vec<_>>(),
            state.slow_polls,
            state.error,
            state.lost,
        ),
        Err(error) => (Vec::new(), Vec::new(), Some(error), true),
    };
    let evidence = json!({"measurement": result.as_ref().ok(), "scenario_error": result.as_ref().err(),
        "cleanup_error": cleanup.as_ref().err(), "receiver_error": receiver_error,
        "receiver_lost": lost, "receiver_slow_polls": gaps, "datagrams": datagrams,
        "serial_counter_observations": observations, "log_directory": case.directory,
        "socket_removed": capture.removed, "notify_socket": capture.path,
        "timestamp_definition": "datagram 时刻为独立收包线程 CLOCK_MONOTONIC_RAW 接收时刻；串口入口为实际调用计数增量的观察上界，入口区间与窗口内收包空档上限均为 50ms。"});
    let path = case.directory.join("watchdog.json");
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(&evidence).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    if result.is_err() || cleanup.is_err() || receiver_error.is_some() || lost || !capture.removed {
        return Err(format!(
            "AC-16 单次串口停滞验收失败，原始证据见 {}",
            path.display()
        ));
    }
    Ok(
        json!({"measurement": result?, "evidence": path, "evidence_sha256": file_hash(&path)?, "log_directory": case.directory}),
    )
}

pub(super) async fn run(args: &Args, report: &mut Report) -> Result<(), String> {
    if args.case.as_deref().is_none_or(|filter| filter == "AC-16") {
        let started = SystemClock.now().0;
        report.record(
            "watchdog_single_serial_call_stall",
            "AC-16",
            started,
            execute(args).await,
        );
    }
    Ok(())
}
