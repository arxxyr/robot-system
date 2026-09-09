//! **AC-E22 pre-increment 崩溃注入**（v0.5 候选 E 段(9)，reviewer round 1
//! P1-6 回修；Spec r11 AB1 第 ⑤ 条）。
//!
//! 同进程单测只能证明"preflight 执行时计数已是 n+1"。真正要锁的性质是**进程
//! 被 SIGKILL 之后**的：
//!
//! 1. 已递增的计数**留在盘上**（不回滚、不被"清理"）；
//! 2. `flock` 由内核在进程死亡时释放，**下一次启动不会误判并发**；
//! 3. 连续 5 次"递增后、完成前崩溃" → 第 6 次启动**直接退 86** 进停止态；
//! 4. 全程 **`s0_daemon` 从未被 exec**。
//!
//! ## 子进程怎么来的
//!
//! 不新增二进制：父测试用 `current_exe()` **重新执行本测试二进制**，只跑
//! [`guard_kill_child`] 这一个 case，并用环境变量把工作目录传进去。子进程里
//! 的 `preflight` 先落一个 marker 再永久阻塞——父进程看到 marker 就
//! `SIGKILL`，从而精确命中"递增之后、两步完成之前"的窗口。

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use robot_s0::deploy::counter::{
    MAX_CONSECUTIVE_FAILURES, acquire_instance_lock, count_path, lock_path, read_count,
};
use robot_s0::deploy::guard::{
    ExecOutcome, GUARD_STOP_EXIT_CODE, GuardConfig, GuardSteps, ProvisionStepError, evaluate_guard,
};
use robot_s0::deploy::types::DeployArm;

/// 子进程模式开关 + 工作目录。
const CHILD_DIR_ENV: &str = "ROBOT_S0_GUARD_KILL_DIR";
/// 子进程在 preflight 里落下的 marker（父进程据此精确择时 `SIGKILL`）。
const PREFLIGHT_MARKER: &str = "preflight_started";
/// 一旦出现即证明 `exec s0_daemon` 被执行过——本套 case 全程不得出现。
const EXEC_MARKER: &str = "exec_called";
/// 单次等待上限（marker 出现 / 子进程退出）。
const WAIT_LIMIT: Duration = Duration::from_secs(20);

const ROBOT_ID: &str = "xlerobot_left";

fn guard_config(dir: &Path) -> GuardConfig {
    GuardConfig {
        arm: DeployArm::Left,
        config_path: dir.join("xlerobot_left_arm.toml"),
        s0_bin: dir.join("s0_daemon"),
        runtime_dir: dir.join("run"),
        robot_id: ROBOT_ID.to_string(),
        s0_args: vec![
            OsString::from("--robot-id"),
            OsString::from(ROBOT_ID),
            OsString::from("--hardware-id"),
            OsString::from("left_arm"),
            OsString::from("--latch-path"),
            dir.join("estop_latch").into_os_string(),
        ],
    }
}

/// 子进程用的 steps：preflight 落 marker 后**永久阻塞**，等父进程 `SIGKILL`。
struct BlockingSteps {
    dir: PathBuf,
}

impl GuardSteps for BlockingSteps {
    fn verify_identity(&mut self, _cfg: &GuardConfig) -> Result<(), String> {
        Ok(())
    }

    fn preflight(&mut self, _cfg: &GuardConfig) -> Result<(), String> {
        std::fs::write(self.dir.join(PREFLIGHT_MARKER), b"1").expect("write marker");
        // 递增已落盘、两步尚未完成——父进程正是在这个窗口里 SIGKILL。
        loop {
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn provision(&mut self, _cfg: &GuardConfig) -> Result<(), ProvisionStepError> {
        Ok(())
    }

    fn exec_s0(&mut self, _cfg: &GuardConfig, _args: &[OsString]) -> ExecOutcome {
        std::fs::write(self.dir.join(EXEC_MARKER), b"1").expect("write marker");
        ExecOutcome::Execed
    }
}

/// 子进程入口：父进程模式下（无环境变量）直接返回，是个空 case。
#[test]
fn guard_kill_child() {
    let Ok(dir) = std::env::var(CHILD_DIR_ENV) else {
        return;
    };
    let dir = PathBuf::from(dir);
    let cfg = guard_config(&dir);
    let mut steps = BlockingSteps { dir };
    let code = match evaluate_guard(&cfg, &mut steps) {
        Ok(()) => 0,
        Err(exit) => i32::from(exit.code()),
    };
    std::process::exit(code);
}

fn wait_for(path: &Path) -> bool {
    let deadline = Instant::now() + WAIT_LIMIT;
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

fn spawn_child(dir: &Path) -> std::process::Child {
    Command::new(std::env::current_exe().expect("current_exe"))
        .args([
            "--exact",
            "guard_kill_child",
            "--nocapture",
            "--test-threads",
            "1",
        ])
        .env(CHILD_DIR_ENV, dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn guard child")
}

/// 连续 5 次"递增后、完成前 `SIGKILL`" → 计数逐次保留、锁每次释放、第 6 次
/// 直接停止态，且 `s0_daemon` 全程从未被 exec。
#[test]
fn sigkill_during_preflight_preserves_count_and_never_execs() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let dir = tmp.path();
    std::fs::create_dir(dir.join("run")).unwrap();
    // s0_bin 必须存在（validate_invocation 前置校验）。
    std::fs::write(dir.join("s0_daemon"), b"#!/bin/true\n").unwrap();

    let cfg = guard_config(dir);
    let count_file = count_path(&cfg.runtime_dir, ROBOT_ID);
    let lock_file = lock_path(&cfg.runtime_dir, ROBOT_ID);
    let marker = dir.join(PREFLIGHT_MARKER);
    let exec_marker = dir.join(EXEC_MARKER);

    for attempt in 1..=MAX_CONSECUTIVE_FAILURES {
        let mut child = spawn_child(dir);
        assert!(
            wait_for(&marker),
            "第 {attempt} 次：preflight marker 未出现（子进程没跑到递增之后）"
        );
        // 精确命中"已递增、未完成"的窗口。
        child.kill().expect("SIGKILL");
        let status = child.wait().expect("wait");
        assert!(
            !status.success(),
            "第 {attempt} 次：被 SIGKILL 不应成功退出"
        );
        std::fs::remove_file(&marker).expect("clear marker");

        assert_eq!(
            read_count(&count_file).expect("read count"),
            attempt,
            "第 {attempt} 次：SIGKILL 必须保留已递增值"
        );
        // 内核在进程死亡时释放 flock —— 下一次启动不得误判并发。
        acquire_instance_lock(&lock_file)
            .unwrap_or_else(|e| panic!("第 {attempt} 次：锁未释放：{e}"))
            .close();
        assert!(
            !exec_marker.exists(),
            "第 {attempt} 次：s0_daemon 绝不允许被 exec"
        );
    }

    // 第 6 次：读到 5 → 直接退 86，连 preflight 都不进。
    let output = spawn_child(dir).wait_with_output().expect("wait");
    assert_eq!(
        output.status.code(),
        Some(i32::from(GUARD_STOP_EXIT_CODE)),
        "第 6 次必须进停止态退 86"
    );
    assert!(!marker.exists(), "停止态下不得执行 preflight");
    assert!(!exec_marker.exists(), "停止态下不得 exec s0_daemon");
    assert_eq!(
        read_count(&count_file).expect("read count"),
        MAX_CONSECUTIVE_FAILURES,
        "停止态不得改动计数"
    );
}
