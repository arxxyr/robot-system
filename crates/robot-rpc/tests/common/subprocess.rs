//! 跨进程 e2e test 用的 `SubprocessHandle` + 子进程 spawn helpers。
//!
//! ## 设计来源
//!
//! 沿 A1 e2e（`crates/robot-tests/src/bin/clear_estop_e2e.rs:415-540`）的
//! `SubprocessHandle` / `spawn_stream_drain` / `wait_for_marker` /
//! `shutdown` 模式；B4 (3/3) 加：
//!
//! - **`kill_on_drop(true)`**（A1 不显式开；B4 reviewer Plan Approved
//!   要求 RAII 兜底）
//! - `dump_stderr_to(path)`（forensic 写到 `/tmp/b4_e2e/<robot_id>/`）
//! - `snapshot_stderr_head_tail(n)`（panic 路径 `eprintln!` 头尾 N 行）
//! - `locate_test_bin(name)`（cargo test 派生 target dir，找
//!   `s1_daemon` / `robot_rpc_gateway` 二进制）
//!
//! ## 子进程 stream drain 策略
//!
//! `Stdio::piped()` + 2 个 tokio task 各 drain stdout / stderr 到共享
//! `Arc<tokio::sync::Mutex<Vec<String>>>`；同时 `eprintln!("[{label}/{stream}]
//! {line}")` 转发让 cargo test 失败时可见（cargo test capture 默认抓
//! eprintln）。pipe buffer 不 drain 会让子进程 write 阻塞。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use thiserror::Error;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

const STREAM_TAG_STDOUT: &str = "stdout";
const STREAM_TAG_STDERR: &str = "stderr";
const MARKER_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// forensic dump 前有界等 drain task 收尾的上限（round 2 P1-4）。
const DRAIN_JOIN_TIMEOUT: Duration = Duration::from_secs(2);

pub struct SubprocessHandle {
    pub label: &'static str,
    pid: Option<u32>,
    child: Option<Child>,
    /// stdout + stderr 合并行（带 `[label/stream]` prefix 已在 drain task 里
    /// 转发，captured 这里只存 raw line）。
    captured: Arc<Mutex<Vec<String>>>,
    /// 两个 drain task 的 handle（v0.5 候选 E 段(7) round 2 P1-4）：
    /// 子进程退出后 pipe 会 EOF，drain task 随之结束——**dump 前必须有界
    /// join 它们**，否则管道尾部（如 SIGTERM handler 的收尾日志）可能还没
    /// 被消费就被快照。
    drains: Vec<tokio::task::JoinHandle<()>>,
}

#[derive(Debug, Error)]
pub enum MarkerError {
    #[error(
        "marker `{needle}` not found in {label} stdout/stderr within {timeout_ms} ms (captured_lines={captured_lines})"
    )]
    Timeout {
        label: &'static str,
        needle: String,
        timeout_ms: u128,
        captured_lines: usize,
    },
    #[error("subprocess {label} exited before marker (exit={status:?})")]
    EarlyExit {
        label: &'static str,
        status: Option<std::process::ExitStatus>,
    },
}

#[derive(Debug, Error)]
pub enum ShutdownError {
    #[error("{label}: SIGTERM signal failed: {error}")]
    Signal {
        label: &'static str,
        error: std::io::Error,
    },
    #[error("{label}: wait timeout after {grace_ms} ms grace; SIGKILL fallback applied")]
    GraceTimeout { label: &'static str, grace_ms: u128 },
    #[error("{label}: child wait error: {error}")]
    Wait {
        label: &'static str,
        error: std::io::Error,
    },
}

impl SubprocessHandle {
    /// Spawn 子进程：`Stdio::piped()` + `kill_on_drop(true)` + 起 2 个 drain
    /// task（stdout / stderr 转 captured + eprintln）。
    ///
    /// # Errors
    ///
    /// `tokio::process::Command::spawn` 失败（极罕见）。
    pub fn spawn(label: &'static str, mut command: Command) -> std::io::Result<Self> {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        // ROBOT_LOG_FORMAT=json + NO_COLOR=1：让日志可按行 substring 匹配
        // marker（不依赖 ANSI 彩色 / 多行 stack）；同 A1 e2e 模式。
        command.env("ROBOT_LOG_FORMAT", "json").env("NO_COLOR", "1");
        // RAII 兜底：test panic 时 Drop 自动 SIGKILL（不依赖手动 shutdown
        // 调用）；reviewer Plan Approved 阶段拍板硬要求。
        command.kill_on_drop(true);

        let mut child = command.spawn()?;
        let pid = child.id();
        let stdout = child.stdout.take().expect("Stdio::piped stdout");
        let stderr = child.stderr.take().expect("Stdio::piped stderr");

        let captured = Arc::new(Mutex::new(Vec::<String>::with_capacity(512)));
        let drains = vec![
            spawn_stream_drain(label, STREAM_TAG_STDOUT, stdout, Arc::clone(&captured)),
            spawn_stream_drain(label, STREAM_TAG_STDERR, stderr, Arc::clone(&captured)),
        ];

        Ok(Self {
            label,
            pid,
            child: Some(child),
            captured,
            drains,
        })
    }

    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Poll captured 行 substring 匹配 `needle`；50ms interval / 总
    /// `timeout` 上限。匹配返 `Ok(())`；timeout 返 `MarkerError::Timeout`。
    /// 子进程已退出（`child.try_wait()` 返 `Some`）但仍未匹配 → `EarlyExit`。
    ///
    /// # Errors
    ///
    /// 见 [`MarkerError`]。
    pub async fn wait_for_marker(
        &mut self,
        needle: &str,
        timeout: Duration,
    ) -> Result<(), MarkerError> {
        let deadline = Instant::now() + timeout;
        let label = self.label;
        loop {
            {
                let lines = self.captured.lock().await;
                if lines.iter().any(|l| l.contains(needle)) {
                    return Ok(());
                }
            }
            // 检查子进程是否已退出（早夭路径）。
            if let Some(child) = self.child.as_mut()
                && let Ok(Some(status)) = child.try_wait()
            {
                return Err(MarkerError::EarlyExit {
                    label,
                    status: Some(status),
                });
            }
            if Instant::now() >= deadline {
                let n = self.captured.lock().await.len();
                return Err(MarkerError::Timeout {
                    label,
                    needle: needle.to_string(),
                    timeout_ms: timeout.as_millis(),
                    captured_lines: n,
                });
            }
            tokio::time::sleep(MARKER_POLL_INTERVAL).await;
        }
    }

    /// 干净 shutdown：SIGTERM → wait `grace` → 超时 SIGKILL → wait。
    /// 返 child exit status 或 [`ShutdownError`]。
    ///
    /// # Errors
    ///
    /// 见 [`ShutdownError`]。
    pub async fn shutdown(
        mut self,
        grace: Duration,
    ) -> Result<std::process::ExitStatus, ShutdownError> {
        let label = self.label;
        let mut child = self
            .child
            .take()
            .expect("SubprocessHandle::shutdown: child already taken");
        // SIGTERM via libc（tokio::process::Child::kill 是 SIGKILL，太硬）。
        if let Some(pid) = self.pid {
            let pid_i32 = i32::try_from(pid).unwrap_or(0);
            // SAFETY: pid 来自 Child::id()，进程仍在或已退出（kill 返 -1
            // 但不 panic）；signal 是 async-signal-safe。
            unsafe { libc::kill(pid_i32, libc::SIGTERM) };
        }
        match tokio::time::timeout(grace, child.wait()).await {
            Ok(Ok(status)) => Ok(status),
            Ok(Err(e)) => Err(ShutdownError::Wait { label, error: e }),
            Err(_elapsed) => {
                // SIGTERM grace 过期：SIGKILL fallback。
                let _ = child.kill().await;
                let kill_result = child.wait().await;
                let _ = kill_result;
                Err(ShutdownError::GraceTimeout {
                    label,
                    grace_ms: grace.as_millis(),
                })
            }
        }
    }

    /// [`Self::shutdown`] + **停机后** forensic dump（段(7) round 1 issue 6，
    /// round 2 P1-4 补齐 drain join）：
    ///
    /// 1. 取出两个 drain task handle 与 `captured` 的 `Arc`；
    /// 2. `shutdown`（SIGTERM → grace → SIGKILL）；
    /// 3. **有界 join 两个 drain task**（子进程退出后 pipe EOF，task 自然
    ///    结束）——不 join 就快照会漏掉管道里尚未被消费的尾部日志；
    /// 4. 写盘。dump 失败只 `eprintln!`，不掩盖 shutdown 的结果。
    ///
    /// # Errors
    ///
    /// 同 [`Self::shutdown`]。
    pub async fn shutdown_with_dump(
        mut self,
        grace: Duration,
        path: PathBuf,
    ) -> Result<std::process::ExitStatus, ShutdownError> {
        let label = self.label;
        let captured = Arc::clone(&self.captured);
        let drains = std::mem::take(&mut self.drains);
        let result = self.shutdown(grace).await;
        // round 2 P1-4：子进程退出 → pipe EOF → drain task 结束。**有界**
        // join 两个 drain，保证 SIGTERM handler 打的收尾行已进 captured；
        // 超时只告警（不掩盖 shutdown 结果），随后照常快照。
        for d in drains {
            match tokio::time::timeout(DRAIN_JOIN_TIMEOUT, d).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => eprintln!("[{label}/forensic] drain task join error: {e}"),
                Err(_elapsed) => eprintln!(
                    "[{label}/forensic] drain task did not finish within {DRAIN_JOIN_TIMEOUT:?}; \
                     dump may miss trailing lines"
                ),
            }
        }
        let lines = captured.lock().await;
        let content = lines.join("\n");
        drop(lines);
        if let Some(parent) = path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        if let Err(e) = tokio::fs::write(&path, content.as_bytes()).await {
            eprintln!("[{label}/forensic] dump to {} failed: {e}", path.display());
        }
        result
    }

    /// Forensic：把 captured 行（含 stdout + stderr）写到 `path`，每行一行。
    ///
    /// # Errors
    ///
    /// 写文件失败。
    pub async fn dump_captured_to(&self, path: &Path) -> std::io::Result<()> {
        let lines = self.captured.lock().await;
        let content = lines.join("\n");
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(path, content.as_bytes()).await
    }

    /// Forensic snapshot：头 N + 尾 N 行（panic 路径 eprintln 用）。
    pub async fn snapshot_head_tail(&self, n: usize) -> Vec<String> {
        let lines = self.captured.lock().await;
        let total = lines.len();
        if total <= 2 * n {
            return lines.clone();
        }
        let mut out = Vec::with_capacity(2 * n + 1);
        out.extend_from_slice(&lines[..n]);
        out.push(format!("... ({} more lines elided) ...", total - 2 * n));
        out.extend_from_slice(&lines[total - n..]);
        out
    }
}

impl Drop for SubprocessHandle {
    fn drop(&mut self) {
        // RAII 兜底：kill_on_drop=true 已在 Command 上设置，child drop =
        // SIGKILL；本 Drop 仅做 PID log（不再 join，避免阻塞 tokio worker）。
        if let Some(pid) = self.pid {
            // panic 路径 cargo test 会捕获 stderr。
            eprintln!(
                "[{label}/drop] pid={pid} kill_on_drop=true; child SIGKILL on Drop",
                label = self.label
            );
        }
    }
}

fn spawn_stream_drain(
    label: &'static str,
    stream_tag: &'static str,
    handle: impl tokio::io::AsyncRead + Unpin + Send + 'static,
    captured: Arc<Mutex<Vec<String>>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut reader = BufReader::new(handle).lines();
        loop {
            match reader.next_line().await {
                Ok(Some(line)) => {
                    eprintln!("[{label}/{stream_tag}] {line}");
                    let mut lines = captured.lock().await;
                    lines.push(line);
                }
                Ok(None) => break,
                Err(e) => {
                    eprintln!("[{label}/{stream_tag}] drain read err: {e}");
                    break;
                }
            }
        }
    })
}

// ============================================================================
// Command builders
// ============================================================================

/// `s0_daemon` Command（v0.5 候选 E 段(7) 六进程拓扑）：`--config` /
/// `--robot-id` / `--hardware-id` / `--latch-path`；latch 必须已 provision
/// 为 `0\n`（strict fail-closed，见 `DualCtx` bring-up 顺序）。
#[must_use]
pub fn build_s0_daemon_command(
    bin_path: &Path,
    robot_id: &str,
    hardware_id: &str,
    config_path: &Path,
    latch_path: &Path,
) -> Command {
    let mut cmd = Command::new(bin_path);
    cmd.arg("--robot-id")
        .arg(robot_id)
        .arg("--hardware-id")
        .arg(hardware_id)
        .arg("--config")
        .arg(config_path)
        .arg("--latch-path")
        .arg(latch_path);
    cmd.env("RUST_LOG", "info");
    cmd
}

/// `s1_daemon` Command：5 args + `RUST_LOG=info` 让 ready marker 必出现。
#[must_use]
pub fn build_s1_daemon_command(
    bin_path: &Path,
    robot_id: &str,
    hardware_id: &str,
    config_path: &Path,
) -> Command {
    let mut cmd = Command::new(bin_path);
    cmd.arg("--robot-id")
        .arg(robot_id)
        .arg("--hardware-id")
        .arg(hardware_id)
        .arg("--config")
        .arg(config_path)
        .arg("--poll-interval-ms")
        .arg("1");
    cmd.env("RUST_LOG", "info");
    cmd
}

/// `robot_rpc_gateway` Command：base 3 args + `RUST_LOG=info`；可选 `--config`
/// （v0.5 候选 B (2/4) state 订阅池来源，D10）。`config_path = None` → 不传
/// `--config`（B4 既有 task_rpc_e2e 行为不变）。
#[must_use]
pub fn build_gateway_command(
    bin_path: &Path,
    robot_id: &str,
    listen_addr: std::net::SocketAddr,
    roles_toml_path: &Path,
    config_path: Option<&Path>,
) -> Command {
    let mut cmd = Command::new(bin_path);
    cmd.arg("--robot-id")
        .arg(robot_id)
        .arg("--listen-addr")
        .arg(listen_addr.to_string())
        .arg("--rpc-roles-toml")
        .arg(roles_toml_path);
    if let Some(cfg) = config_path {
        cmd.arg("--config").arg(cfg);
    }
    cmd.env("RUST_LOG", "info");
    cmd
}

/// 找 cargo workspace target 目录下的二进制（release 优先 → debug fallback）。
///
/// `CARGO_BIN_EXE_robot_rpc_gateway` 由 cargo test runner 设置（仅 robot-rpc
/// 自身的 [[bin]]）；其它 crate（如 robot-s1）的 `s1_daemon` 不在该 env
/// 下，需要从 target dir 派生。本 fn 用 gateway path 的祖父目录 = workspace
/// `target/`，再尝试 `release/<name>` / `debug/<name>`。
///
/// # Errors
///
/// - `CARGO_BIN_EXE_robot_rpc_gateway` 未设置（cargo test 之外的环境）
/// - 二进制不存在（dev 没跑 `cargo build --release --workspace`）
pub fn locate_test_bin(name: &str) -> Result<PathBuf, String> {
    // 允许显式 env override（CI / 自定义 target dir 用）。
    let env_key = format!("B4_E2E_BIN_{}", name.to_uppercase());
    if let Ok(p) = std::env::var(&env_key) {
        let path = PathBuf::from(p);
        if path.exists() {
            return Ok(path);
        }
        return Err(format!(
            "env override {env_key} = {} 不存在；请确认 cargo build --release",
            path.display()
        ));
    }

    // CARGO_BIN_EXE_robot_rpc_gateway 由 cargo test runner 注入（compile time
    // env）；这里 std::env::var 拿 runtime 同名 env（cargo test 也会同名注入
    // 给 test binary）。两路一致；用 std::env::var 让 fn 可被非 build-script
    // / 非 macro 路径调用。
    let gateway = std::env::var("CARGO_BIN_EXE_robot_rpc_gateway").map_err(|_| {
        "CARGO_BIN_EXE_robot_rpc_gateway 未设置（仅 cargo test 环境可用）".to_string()
    })?;
    let bin_dir = Path::new(&gateway)
        .parent()
        .ok_or("无法从 CARGO_BIN_EXE_robot_rpc_gateway 派生 bin_dir")?;
    let target_dir = bin_dir.parent().ok_or("无法从 bin_dir 派生 target_dir")?;

    for profile in ["release", "debug"] {
        let path = target_dir.join(profile).join(name);
        if path.exists() {
            return Ok(path);
        }
    }
    Err(format!(
        "二进制 {name:?} 在 {}/{{release,debug}}/ 都不存在；\
         请先 `cargo build --release --workspace`",
        target_dir.display()
    ))
}

// ============================================================================
// Tests（round 2 P1-4：dump 必须含停机期输出）
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// SIGTERM handler 打尾部 marker 的子进程：验证 `shutdown_with_dump` 的
    /// dump 里**确实包含**停机期输出（不 join drain task 时该行会丢）。
    #[tokio::test]
    async fn shutdown_dump_contains_sigterm_handler_output() {
        const MARKER: &str = "E2E_SIGTERM_TAIL_MARKER";
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(format!(
            "trap 'echo {MARKER}; exit 0' TERM; echo started; while true; do sleep 0.05; done"
        ));
        let mut child = SubprocessHandle::spawn("dump_test", cmd).expect("spawn sh");
        child
            .wait_for_marker("started", Duration::from_secs(5))
            .await
            .expect("child started");

        let dir = std::env::temp_dir().join(format!("dimos_dump_test_{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.expect("mkdir");
        let path = dir.join("dump.log");
        let status = child
            .shutdown_with_dump(Duration::from_secs(3), path.clone())
            .await
            .expect("graceful shutdown");
        assert!(
            status.success(),
            "SIGTERM handler 应 exit 0，实得 {status:?}"
        );

        let dumped = tokio::fs::read_to_string(&path).await.expect("read dump");
        assert!(
            dumped.contains(MARKER),
            "dump 必须含 SIGTERM handler 的尾部输出；实得:\n{dumped}"
        );
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
