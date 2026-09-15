//! manager 操作子进程的有限拥有域；daemon 本身始终归 manager 回收。

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use robot_core::time::{RobotClock, SystemClock};
use serde_json::{Value, json};
use tokio::process::{Child, Command};

use super::write_json;

#[derive(Debug)]
pub(super) struct Output {
    pub(super) status: ExitStatus,
    pub(super) stdout: String,
    pub(super) evidence: Value,
}

impl Output {
    pub(super) fn require_success(self) -> Result<Self, String> {
        if self.status.success() {
            Ok(self)
        } else {
            Err(format!("原命令失败：{}", self.evidence))
        }
    }
}

pub(super) struct Runner {
    directory: PathBuf,
    sequence: u64,
}

impl Runner {
    pub(super) fn new(directory: &Path) -> Result<Self, String> {
        std::fs::create_dir(directory).map_err(|error| error.to_string())?;
        Ok(Self {
            directory: directory.to_owned(),
            sequence: 0,
        })
    }

    pub(super) async fn run(
        &mut self,
        program: &Path,
        args: &[String],
        until: u64,
    ) -> Result<Output, String> {
        self.sequence = self.sequence.checked_add(1).ok_or("命令序号溢出")?;
        let prefix = self.directory.join(format!("{:05}", self.sequence));
        let stdout_path = prefix.with_extension("stdout");
        let stderr_path = prefix.with_extension("stderr");
        let stdout = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&stdout_path)
            .map_err(|error| error.to_string())?;
        let stderr = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&stderr_path)
            .map_err(|error| error.to_string())?;
        let started = SystemClock.now().0;
        if started >= until {
            return Err("原命令在绝对截止后不得再启动".into());
        }
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .env("LC_ALL", "C")
            .env("SYSTEMD_COLORS", "0")
            .env("SYSTEMD_PAGER", "cat")
            .kill_on_drop(true);
        let spawned = command.spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(error) => {
                write_json(
                    &prefix.with_extension("json"),
                    &json!({
                        "program":program,"argv":args,"started_ns":started,"deadline_ns":until,
                        "spawn_error":error.to_string()
                    }),
                )?;
                return Err(format!("原命令spawn失败：{error}"));
            }
        };
        // 成功 spawn 后立即拥有原 Child；后面的任何错误均先真实回收。
        let pid = child.id();
        let waited = wait_owned(&mut child, until).await;
        let at_ns = SystemClock.now().0;
        let evidence = json!({"program":program,"argv":args,"pid":pid,
            "started_ns":started,"deadline_ns":until,"observed_at_ns":at_ns,
            "wait":waited.as_ref().map(ToString::to_string),
            "stdout":stdout_path,"stderr":stderr_path});
        write_json(&prefix.with_extension("json"), &evidence)?;
        let status = waited?;
        let stdout = std::fs::read_to_string(stdout_path).map_err(|error| error.to_string())?;
        Ok(Output {
            status,
            stdout,
            evidence,
        })
    }

    pub(super) async fn sha256(&mut self, path: &Path) -> Result<String, String> {
        let output = self
            .run(
                Path::new("/usr/bin/sha256sum"),
                &[path.to_string_lossy().into_owned()],
                SystemClock.now().0 + 3_000_000_000,
            )
            .await?
            .require_success()?;
        let hash = output
            .stdout
            .split_whitespace()
            .next()
            .ok_or("缺SHA256输出")?;
        if hash.len() != 64
            || !hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err("原SHA256输出非法".into());
        }
        Ok(hash.to_owned())
    }
}

async fn wait_owned(child: &mut Child, until: u64) -> Result<ExitStatus, String> {
    let first_error = loop {
        let polled = child.try_wait();
        let observed = SystemClock.now().0;
        match polled {
            Ok(Some(status)) if observed < until => return Ok(status),
            Ok(Some(status)) => {
                return Err(format!(
                    "原命令已真reap但越过截止：{status}，at={observed}，deadline={until}"
                ));
            }
            Ok(None) if observed < until => tokio::time::sleep(Duration::from_millis(2)).await,
            Ok(None) => break "原命令超过绝对截止".to_owned(),
            Err(error) => break format!("原命令poll失败：{error}"),
        }
    };
    let kill = child.start_kill();
    let kill_return_ns = SystemClock.now().0;
    let reap_until = kill_return_ns + 2_000_000_000;
    loop {
        let polled = child.try_wait();
        let at_ns = SystemClock.now().0;
        match polled {
            Ok(Some(status)) => {
                return Err(format!(
                    "{first_error}；kill={kill:?}；kill_return_ns={kill_return_ns}；真reap={status}；reaped_ns={at_ns}；within_2s={}",
                    at_ns < reap_until
                ));
            }
            Ok(None) if at_ns < reap_until => tokio::time::sleep(Duration::from_millis(2)).await,
            other => {
                eprintln!(
                    "manager命令原Child未能真回收，禁止丢失拥有权：{first_error}；kill={kill:?}；poll={other:?}"
                );
                std::process::abort();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn real_command_timeout_kills_and_reaps_original_child_with_raw_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let mut runner = Runner::new(&directory.path().join("commands")).unwrap();
        let error = runner
            .run(
                Path::new("/bin/sleep"),
                &["10".into()],
                SystemClock.now().0 + 200_000_000,
            )
            .await
            .expect_err("原sleep必须超时");
        assert!(error.contains("真reap="));
        let evidence: Value = serde_json::from_slice(
            &std::fs::read(directory.path().join("commands/00001.json")).unwrap(),
        )
        .unwrap();
        assert!(
            evidence["wait"]["Err"]
                .as_str()
                .unwrap()
                .contains("within_2s=true")
        );
        assert!(evidence["pid"].as_u64().unwrap() > 0);
        assert!(directory.path().join("commands/00001.stdout").is_file());
        assert!(directory.path().join("commands/00001.stderr").is_file());
    }

    #[tokio::test]
    async fn real_nonzero_command_is_reaped_and_cannot_be_counted_as_success() {
        let directory = tempfile::tempdir().unwrap();
        let mut runner = Runner::new(&directory.path().join("commands")).unwrap();
        let output = runner
            .run(
                Path::new("/bin/false"),
                &[],
                SystemClock.now().0 + 2_000_000_000,
            )
            .await
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(output.require_success().is_err());
    }
}
