//! S48 A3d / W1：无硬件夹具验证 `xlerobot_deploy provision` 子命令的退出码映射。
//!
//! 只跑真实 CLI 二进制（`required-features = ["xlerobot-real"]`，与 bin 同门），
//! 用临时目录的 `--latch-path` 夹具，不打开任何串口、不接触 `/var/lib/robot`：
//!
//! - 锁被另一进程（本测试进程）持有 → **89**，不写任何文件、不改持有者 PID；
//! - latch 名字 / 现存对象身份错误 → **1**，副作用前拒绝、不自愈；
//! - 原三分支业务失败（损坏副本）→ **71**，不得创建另一份；
//! - 成功 → **0**，双份 `1\n`、锁文件保留且记录 CLI 进程 PID；
//! - 缺少目标参数 → **2**（既有用法错误码）。

#![cfg(all(target_os = "linux", feature = "xlerobot-real"))]

use std::path::Path;
use std::process::{Command, Stdio};

use robot_s0::latch_lock::acquire_latch_lock;

fn run_provision(args: &[&std::ffi::OsStr]) -> (i32, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_xlerobot_deploy"))
        .arg("provision")
        .args(args)
        .env("RUST_LOG", "info")
        .env_remove("NOTIFY_SOCKET")
        .stdin(Stdio::null())
        .output()
        .expect("spawn xlerobot_deploy");
    let code = output
        .status
        .code()
        .unwrap_or_else(|| panic!("被信号终止: {}", output.status));
    let mut text = String::from_utf8_lossy(&output.stderr).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stdout));
    (code, text)
}

fn provision_at(latch: &Path) -> (i32, String) {
    run_provision(&[std::ffi::OsStr::new("--latch-path"), latch.as_os_str()])
}

fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// 成功：双缺失 → 双份 `1\n`，锁文件保留并记录 CLI 进程 PID（不是本测试进程）。
#[test]
fn provision_success_exits_0_and_writes_latched_pair() {
    let dir = tempfile::TempDir::new().unwrap();
    let latch = dir.path().join("estop_latch");
    let (code, log) = provision_at(&latch);
    assert_eq!(code, 0, "{log}");
    assert!(log.contains("provision: ok"), "{log}");
    assert_eq!(std::fs::read_to_string(&latch).unwrap(), "1\n");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("estop_latch.bak")).unwrap(),
        "1\n"
    );
    assert_eq!(
        entries(dir.path()),
        ["estop_latch", "estop_latch.bak", "estop_latch.lock"]
    );
    let recorded: u32 = std::fs::read_to_string(dir.path().join("estop_latch.lock"))
        .unwrap()
        .trim()
        .parse()
        .expect("锁文件记录 PID");
    assert_ne!(recorded, std::process::id(), "PID 应是 CLI 进程的");
    // CLI 退出后锁已由内核释放。
    drop(acquire_latch_lock(&latch).expect("CLI 退出后可重取"));
}

/// 锁被本测试进程持有 → 89；不写双份、不改 PID。
#[test]
fn held_lock_exits_89_without_side_effects() {
    let dir = tempfile::TempDir::new().unwrap();
    let latch = dir.path().join("estop_latch");
    let _holder = acquire_latch_lock(&latch).unwrap();
    let (code, log) = provision_at(&latch);
    assert_eq!(code, 89, "{log}");
    assert!(
        log.contains("provision: FAIL") && log.contains("code=89"),
        "{log}"
    );
    assert_eq!(entries(dir.path()), ["estop_latch.lock"]);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("estop_latch.lock")).unwrap(),
        format!("{}\n", std::process::id())
    );
}

/// latch 名字为派生名 / RX 固定名 → 1，目录不出现任何文件。
#[test]
fn invalid_latch_name_exits_1_before_any_write() {
    let dir = tempfile::TempDir::new().unwrap();
    for bad in ["estop_latch.bak", "estop_latch.lock", "rx_trust_marker"] {
        let (code, log) = provision_at(&dir.path().join(bad));
        assert_eq!(code, 1, "{bad}: {log}");
        assert!(log.contains("code=1"), "{log}");
    }
    assert!(entries(dir.path()).is_empty(), "不得创建任何文件");
}

/// 现存 primary 是符号链接 → 1，不补写 backup、链接目标不变。
#[test]
fn symlink_primary_exits_1_without_self_heal() {
    let dir = tempfile::TempDir::new().unwrap();
    let real = dir.path().join("real_latch");
    std::fs::write(&real, "1\n").unwrap();
    let latch = dir.path().join("estop_latch");
    std::os::unix::fs::symlink("real_latch", &latch).unwrap();
    let (code, log) = provision_at(&latch);
    assert_eq!(code, 1, "{log}");
    assert!(!dir.path().join("estop_latch.bak").exists());
    assert_eq!(std::fs::read_to_string(&real).unwrap(), "1\n");
}

/// 原业务失败：现存唯一副本损坏 → 71，不得创建另一份。
#[test]
fn corrupt_existing_copy_exits_71_and_creates_nothing() {
    let dir = tempfile::TempDir::new().unwrap();
    let latch = dir.path().join("estop_latch");
    std::fs::write(&latch, "garbage").unwrap();
    let (code, log) = provision_at(&latch);
    assert_eq!(code, 71, "{log}");
    assert!(log.contains("code=71"), "{log}");
    assert!(!dir.path().join("estop_latch.bak").exists());
    assert_eq!(std::fs::read_to_string(&latch).unwrap(), "garbage");
}

/// 缺少 `--latch-path` / `--robot-id` / `--config` → 既有用法错误码 2。
#[test]
fn missing_target_is_a_usage_error() {
    let (code, _log) = run_provision(&[]);
    assert_eq!(code, 2);
}
