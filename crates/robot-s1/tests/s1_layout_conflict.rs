//! 段(4)：`s1_daemon` 显式 `--layout-id` 与 config 派生冲突 → **exit 2**
//! bin 层 fail-fast（PD-E5(a)：config 派生唯一权威，显式传值仅一致性断言）。
//!
//! 沿既有 e2e 启动失败断言模式：spawn 真 binary（`CARGO_BIN_EXE_`）+
//! 断言退出码与 stderr 文案。冲突检查在 iceoryx2 open / anchor 之前，
//! 失败快速且确定（无 IPC 环境依赖）。tempdir 自己滚（本仓 robot-tests
//! 同款方针，不引 tempfile dev-dep）。

use std::path::PathBuf;
use std::process::Command;

fn scratch_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("s1_layout_conflict_{}_{tag}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn explicit_layout_id_conflict_exits_2() {
    let dir = scratch_dir("conflict");
    let config = dir.join("config.toml");
    std::fs::write(
        &config,
        "[robot]\nid = \"lc_robot\"\nconfig_version = \"0.5\"\n\n\
         [[hardware]]\nid = \"arm\"\nadapter = \"mock_hard_rt\"\n\
         joints = [\"j0\", \"j1\"]\nmotor_ids = [1, 2]\n",
    )
    .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_s1_daemon"))
        .arg("--robot-id")
        .arg("lc_robot")
        .arg("--hardware-id")
        .arg("arm")
        .arg("--config")
        .arg(&config)
        .arg("--layout-id")
        .arg("0x1")
        .output()
        .expect("spawn s1_daemon");

    assert_eq!(
        out.status.code(),
        Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("config 派生"), "stderr: {stderr}");

    let _ = std::fs::remove_dir_all(&dir);
}
