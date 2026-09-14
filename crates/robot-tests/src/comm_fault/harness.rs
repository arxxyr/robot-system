//! 隔离子进程和日志；每个场景单独 robot id、目录、IPC 名称与锁。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use robot_core::time::{RobotClock, SystemClock};
use robot_s0::comm_fault_observation::{ObservationPage, ObservationSnapshot};
use robot_s0::comm_fault_trace::TracePage;
use tokio::process::{Child, Command};

static NEXT_CASE: AtomicU32 = AtomicU32::new(0);

pub(super) fn isolated_command(path: &Path) -> Command {
    let mut command = Command::new(path);
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("ROBOT_TEST_") {
            command.env_remove(key);
        }
    }
    command
        .env_remove("NOTIFY_SOCKET")
        .env_remove("INVOCATION_ID")
        .env_remove("WATCHDOG_USEC")
        .env_remove("WATCHDOG_PID")
        .kill_on_drop(true);
    command
}

pub(super) struct Case {
    pub(super) directory: PathBuf,
    pub(super) config: PathBuf,
    pub(super) robot_id: String,
    pub(super) latch: PathBuf,
    pub(super) trust: PathBuf,
    pub(super) bin_dir: PathBuf,
}

impl Case {
    pub(super) fn new(
        evidence: &Path,
        bin_dir: &Path,
        label: &str,
        joints: usize,
    ) -> Result<Self, String> {
        let sequence = NEXT_CASE.fetch_add(1, Ordering::Relaxed);
        let robot_id = format!("cf_{}_{}", std::process::id(), sequence);
        let directory = evidence.join(format!("{sequence:03}_{label}"));
        std::fs::create_dir(&directory)
            .map_err(|error| format!("创建独立场景目录失败：{error}"))?;
        let trust = directory.join("trust");
        std::fs::create_dir(&trust).map_err(|error| error.to_string())?;
        let latch = directory.join("estop_latch");
        for path in [&latch, &directory.join("estop_latch.bak")] {
            std::fs::write(path, "0\n").map_err(|error| error.to_string())?;
        }
        let joint_names: Vec<String> = (0..joints).map(|index| format!("j{index}")).collect();
        let ids: Vec<usize> = (1..=joints).collect();
        let config = directory.join("robot.toml");
        let content = format!(
            "[robot]\nid = {robot_id:?}\nconfig_version = \"0.1.2\"\n\n[[hardware]]\nid = \"arm\"\nadapter = \"mock_serial_rt\"\njoints = {joint_names:?}\nmotor_ids = {ids:?}\nauto_enable = false\n"
        );
        std::fs::write(&config, content).map_err(|error| error.to_string())?;
        Ok(Self {
            directory,
            config,
            robot_id,
            latch,
            trust,
            bin_dir: bin_dir.to_owned(),
        })
    }

    pub(super) fn command(&self) -> Command {
        let mut command = isolated_command(&self.bin_dir.join("s0_daemon"));
        command
            .args(["--config"])
            .arg(&self.config)
            .args(["--hardware-id", "arm", "--latch-path"])
            .arg(&self.latch)
            .arg("--rx-trust-dir")
            .arg(&self.trust)
            .env_remove("NOTIFY_SOCKET")
            .env_remove("INVOCATION_ID")
            .kill_on_drop(true);
        command
    }

    pub(super) fn spawn(&self, name: &str, mut command: Command) -> Result<Child, String> {
        let observation_path = self.directory.join(format!("{name}.observation"));
        let _page = ObservationPage::create(&observation_path)
            .map_err(|error| format!("创建观测页失败：{error}"))?;
        let trace_path = self.directory.join(format!("{name}.trace"));
        let _trace =
            TracePage::create(&trace_path).map_err(|error| format!("创建事件环失败：{error}"))?;
        let stdout = std::fs::File::create(self.directory.join(format!("{name}.stdout.log")))
            .map_err(|error| error.to_string())?;
        let stderr = std::fs::File::create(self.directory.join(format!("{name}.stderr.log")))
            .map_err(|error| error.to_string())?;
        command
            .env("ROBOT_TEST_OBSERVATION_PATH", &observation_path)
            .env("ROBOT_TEST_TRACE_PATH", &trace_path)
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
        command
            .spawn()
            .map_err(|error| format!("启动 {name} 失败：{error}"))
    }

    pub(super) fn logs(&self, name: &str) -> Result<String, String> {
        let mut combined =
            std::fs::read_to_string(self.directory.join(format!("{name}.stdout.log")))
                .map_err(|error| error.to_string())?;
        combined.push_str(
            &std::fs::read_to_string(self.directory.join(format!("{name}.stderr.log")))
                .map_err(|error| error.to_string())?,
        );
        Ok(combined)
    }

    pub(super) fn observation(&self, name: &str) -> Result<ObservationSnapshot, String> {
        ObservationPage::open(&self.directory.join(format!("{name}.observation")))
            .map(|page| page.snapshot())
            .map_err(|error| format!("读取观测页失败：{error}"))
    }

    pub(super) fn trace(&self, name: &str) -> Result<TracePage, String> {
        TracePage::open(&self.directory.join(format!("{name}.trace")))
            .map_err(|error| error.to_string())
    }
}

pub(super) async fn wait_exit(child: &mut Child, bound: Duration) -> Result<i32, String> {
    let status = tokio::time::timeout(bound, child.wait())
        .await
        .map_err(|_| format!("子进程未在 {bound:?} 内退出"))?
        .map_err(|error| error.to_string())?;
    status
        .code()
        .ok_or_else(|| format!("子进程由信号终止：{status}"))
}

pub(super) async fn stop(child: &mut Child) -> Result<(), String> {
    let Some(pid) = child.id() else {
        return Ok(());
    };
    let pid = i32::try_from(pid).map_err(|error| error.to_string())?;
    // SAFETY：PID 来自本场景仍拥有的子进程；只向该进程请求正常退出。
    let result = unsafe { libc::kill(pid, libc::SIGTERM) };
    if result != 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let status = wait_exit(child, Duration::from_secs(5)).await?;
    if status != 0 {
        return Err(format!("正常关闭返回 {status}"));
    }
    Ok(())
}

pub(super) async fn wait_log(
    case: &Case,
    child: &mut Child,
    name: &str,
    needle: &str,
    bound: Duration,
) -> Result<(), String> {
    let until = SystemClock
        .now()
        .0
        .saturating_add(u64::try_from(bound.as_nanos()).unwrap_or(u64::MAX));
    loop {
        if case.logs(name)?.contains(needle) {
            return Ok(());
        }
        if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
            return Err(format!("等待 {needle:?} 时子进程提前退出：{status}"));
        }
        if SystemClock.now().0 >= until {
            return Err(format!("等待日志 {needle:?} 超时"));
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolated_command_removes_all_manager_variables_but_allows_explicit_fixture_values() {
        let mut command = isolated_command(Path::new("/bin/true"));
        for key in [
            "NOTIFY_SOCKET",
            "INVOCATION_ID",
            "WATCHDOG_USEC",
            "WATCHDOG_PID",
        ] {
            assert_eq!(
                command
                    .as_std()
                    .get_envs()
                    .find(|(name, _)| *name == key)
                    .map(|(_, value)| value),
                Some(None)
            );
        }
        command
            .env("NOTIFY_SOCKET", "/tmp/fixture-notify")
            .env("WATCHDOG_USEC", "1000000");
        assert_eq!(
            command
                .as_std()
                .get_envs()
                .find(|(name, _)| *name == "WATCHDOG_USEC")
                .and_then(|(_, value)| value),
            Some(std::ffi::OsStr::new("1000000"))
        );
        assert_eq!(
            command
                .as_std()
                .get_envs()
                .find(|(name, _)| *name == "WATCHDOG_PID")
                .map(|(_, value)| value),
            Some(None)
        );
    }
}
