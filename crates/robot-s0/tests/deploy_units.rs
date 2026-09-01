//! **unit 文件指令文本断言**（v0.5 候选 E 段(9)，AC-E22 unit 级）。
//!
//! 锁的是 systemd 侧的**部署契约**，不碰真硬件：
//!
//! - unit ×7 齐全（handoff + arm S0 ×2 + S1 ×2 + gateway ×2）；
//! - arm S0 unit **无 `ExecStartPre=`**（QA1 / Y1：guard 必须是主进程，
//!   否则 `RestartPreventExitStatus=86` 捕不到停止态）；
//! - **R10-1 三处 config 路径一致锁**：handoff 的双 `--config` / arm guard
//!   的 `--config` / guard **自动追加**给 `s0_daemon` 的 `--config`——用
//!   真的 [`build_s0_args`] 跑一遍，不是字符串比对；
//! - `--` 之后自带 `--config` 的拒绝 case（退 86）；
//! - 依赖图（Requires / Wants / After）、`RuntimeDirectory` 三件套、
//!   两档预算指令、`WatchdogSec` / `Restart` 全套；
//! - **`TimeoutStartSec=infinity`**（round 1 P0-2）：有限值会在"部分电机
//!   已写、其余尚未处理"时截断 ensure-safe 的末尾聚合；
//! - **生产 `WatchdogSec` = 5s**（S1 / gateway，Spec D12 + PX-16；S0 与
//!   freeze 测试模板保持 2s）。

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use robot_s0::deploy::guard::{GUARD_STOP_EXIT_CODE, GuardConfig, GuardExit, build_s0_args};
use robot_s0::deploy::types::DeployArm;

const HANDOFF: &str = "xlerobot-handoff.service";
const ARM_UNITS: [(&str, &str); 2] = [
    ("left", "xlerobot-s0-left-arm.service"),
    ("right", "xlerobot-s0-right-arm.service"),
];
const S1_UNITS: [(&str, &str); 2] = [
    ("left", "xlerobot-s1-left.service"),
    ("right", "xlerobot-s1-right.service"),
];
const GATEWAY_UNITS: [(&str, &str, &str); 2] = [
    ("left", "xlerobot-gateway-left.service", "127.0.0.1:50051"),
    ("right", "xlerobot-gateway-right.service", "127.0.0.1:50052"),
];
/// 旧栈四件（dimos ×2 + 旧 full S0 ×2）——handoff 与 arm unit 都要互斥。
const LEGACY_UNITS: [&str; 4] = [
    "xlerobot-coordinator.service",
    "xlerobot-web.service",
    "xlerobot-s0-bus1.service",
    "xlerobot-s0-bus2.service",
];

fn systemd_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts/systemd")
        .canonicalize()
        .expect("scripts/systemd 必须存在")
}

/// 读 unit 并做**逻辑行**归一：去注释、把反斜杠续行拼回一行。
fn unit_lines(name: &str) -> Vec<String> {
    let raw = std::fs::read_to_string(systemd_dir().join(name))
        .unwrap_or_else(|e| panic!("read {name}: {e}"));
    let mut lines = Vec::new();
    let mut pending = String::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if pending.is_empty() && (trimmed.is_empty() || trimmed.starts_with('#')) {
            continue;
        }
        if let Some(head) = trimmed.strip_suffix('\\') {
            pending.push_str(head.trim_end());
            pending.push(' ');
            continue;
        }
        pending.push_str(trimmed);
        lines.push(std::mem::take(&mut pending));
    }
    assert!(pending.is_empty(), "{name}: 续行未闭合");
    lines
}

/// 取某个指令的全部取值（同名指令可重复，如 `After=`）。
fn directives(name: &str, key: &str) -> Vec<String> {
    unit_lines(name)
        .into_iter()
        .filter_map(|l| l.strip_prefix(&format!("{key}=")).map(str::to_owned))
        .collect()
}

fn single(name: &str, key: &str) -> String {
    let values = directives(name, key);
    assert_eq!(values.len(), 1, "{name}: {key} 应恰好一条，实际 {values:?}");
    values.into_iter().next().unwrap()
}

fn exec_start_argv(name: &str) -> Vec<String> {
    single(name, "ExecStart")
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

fn assert_contains_all(name: &str, key: &str, needles: &[&str]) {
    let joined = directives(name, key).join(" ");
    for needle in needles {
        assert!(
            joined.contains(needle),
            "{name}: {key} 缺 {needle}（{joined}）"
        );
    }
}

/// argv 里 `--key` 的取值。
fn opt(argv: &[String], key: &str) -> String {
    let idx = argv
        .iter()
        .position(|a| a == key)
        .unwrap_or_else(|| panic!("argv 缺 {key}: {argv:?}"));
    argv.get(idx + 1)
        .unwrap_or_else(|| panic!("{key} 缺取值"))
        .clone()
}

/// `--` 之后原样透传给 `s0_daemon` 的部分。
fn s0_args_of(argv: &[String]) -> Vec<String> {
    let idx = argv
        .iter()
        .position(|a| a == "--")
        .expect("arm unit 必须有 `--` 分界");
    argv[idx + 1..].to_vec()
}

// ============================================================================
// 齐全性
// ============================================================================

#[test]
fn seven_units_exist() {
    let mut names = vec![HANDOFF.to_string()];
    names.extend(ARM_UNITS.iter().map(|(_, n)| (*n).to_string()));
    names.extend(S1_UNITS.iter().map(|(_, n)| (*n).to_string()));
    names.extend(GATEWAY_UNITS.iter().map(|(_, n, _)| (*n).to_string()));
    assert_eq!(names.len(), 7, "unit ×7（撤『六件』笔误）");
    for name in names {
        assert!(systemd_dir().join(&name).is_file(), "缺 unit {name}");
    }
    assert!(
        systemd_dir().join("README_dimos_replace.md").is_file(),
        "缺 runbook"
    );
}

// ============================================================================
// handoff unit
// ============================================================================

#[test]
fn handoff_is_oneshot_remain_after_exit_and_conflicts_legacy() {
    assert_eq!(single(HANDOFF, "Type"), "oneshot");
    assert_eq!(single(HANDOFF, "RemainAfterExit"), "yes");
    assert_contains_all(HANDOFF, "Conflicts", &LEGACY_UNITS);
    // 旧栈停止顺序的第一责任人：conflict 目标同时要排 After。
    assert_contains_all(HANDOFF, "After", &LEGACY_UNITS);
}

#[test]
fn handoff_exec_start_is_preflight_handoff_with_both_configs() {
    let argv = exec_start_argv(HANDOFF);
    assert!(argv[0].ends_with("xlerobot_deploy"), "{argv:?}");
    assert_eq!(argv[1], "preflight-handoff");
    assert_eq!(opt(&argv, "--left-config"), left_config_literal());
    assert_eq!(opt(&argv, "--right-config"), right_config_literal());
}

fn left_config_literal() -> String {
    "/etc/robot/xlerobot_left_arm.toml".to_string()
}

fn right_config_literal() -> String {
    "/etc/robot/xlerobot_right_arm.toml".to_string()
}

// ============================================================================
// arm S0 unit（guard 主进程）
// ============================================================================

#[test]
fn arm_units_have_no_exec_start_pre_and_run_guard() {
    for (side, name) in ARM_UNITS {
        assert!(
            directives(name, "ExecStartPre").is_empty(),
            "{name}: 不得有 ExecStartPre（86 只对主进程生效）"
        );
        let argv = exec_start_argv(name);
        assert!(argv[0].ends_with("xlerobot_deploy"), "{name}: {argv:?}");
        assert_eq!(argv[1], "guard", "{name}");
        assert_eq!(opt(&argv, "--arm"), side, "{name}");
        assert!(
            Path::new(&opt(&argv, "--s0-bin")).is_absolute(),
            "{name}: --s0-bin 必须绝对路径"
        );
        assert!(opt(&argv, "--s0-bin").ends_with("s0_daemon"), "{name}");
        assert_eq!(single(name, "Type"), "notify", "{name}");
    }
}

#[test]
fn arm_units_lock_exit_86_and_both_budgets() {
    for (_, name) in ARM_UNITS {
        assert_eq!(
            single(name, "RestartPreventExitStatus"),
            GUARD_STOP_EXIT_CODE.to_string(),
            "{name}"
        );
        assert_eq!(single(name, "StartLimitIntervalSec"), "60s", "{name}");
        assert_eq!(single(name, "StartLimitBurst"), "5", "{name}");
        assert_eq!(single(name, "Restart"), "on-failure", "{name}");
        assert_eq!(single(name, "RestartSec"), "2s", "{name}");
        assert_eq!(single(name, "WatchdogSec"), "2s", "{name}");
    }
}

/// **round 1 P0-2**：D13 明确"给 preflight 定义可证明总时限"这一备选**未
/// 采**——有限 `TimeoutStartSec` 会在部分电机已写、其余尚未处理时杀掉进程，
/// 破坏"全部电机继续处理、末尾聚合"的 fail-closed 语义。三份跑 ensure-safe
/// 的 unit 必须 `infinity`。
#[test]
fn ensure_safe_units_never_time_out_mid_aggregation() {
    for name in [HANDOFF, ARM_UNITS[0].1, ARM_UNITS[1].1] {
        assert_eq!(
            single(name, "TimeoutStartSec"),
            "infinity",
            "{name}: 有限值会截断 ensure-safe 聚合"
        );
    }
}

/// **调度硬化（2026-09-01）**：arm unit 必须放行 RT——`RestrictRealtime`
/// 不得存在（seccomp 拦 RT 调度类，与 `--serial-fifo` 互斥）、
/// `LimitRTPRIO=99` + `LimitMEMLOCK=infinity`、进程掩码 `CPUAffinity=0-3`
/// （普通线程圈定杂务核；线程可 `sched_setaffinity` 越出继承掩码，serial
/// 线程自行钉隔离核——继承掩码是初值不是上限，硬上限只有 cgroup cpuset，
/// 故不得设 `AllowedCPUs=`）；s0 args 携 `--serial-fifo 70` + 本侧
/// `--cpu 4|5` + `--strict-rt`。
#[test]
fn arm_units_grant_rt_and_pass_serial_fifo_flags() {
    for (side, name, cpu) in [
        ("left", ARM_UNITS[0].1, "4"),
        ("right", ARM_UNITS[1].1, "5"),
    ] {
        assert!(
            directives(name, "RestrictRealtime").is_empty(),
            "{name}: RestrictRealtime 与 --serial-fifo 互斥，必须移除"
        );
        assert_eq!(single(name, "LimitRTPRIO"), "99", "{name}");
        assert_eq!(single(name, "LimitMEMLOCK"), "infinity", "{name}");
        assert_eq!(single(name, "CPUAffinity"), "0-3", "{name}");
        let argv = exec_start_argv(name);
        let s0_args = s0_args_of(&argv);
        let has = |flag: &str| s0_args.iter().any(|a| a == flag);
        assert!(has("--serial-fifo"), "{name}");
        assert!(has("--strict-rt"), "{name}");
        let cpu_idx = s0_args.iter().position(|a| a == "--cpu").expect("--cpu");
        assert_eq!(s0_args[cpu_idx + 1], cpu, "{name}: {side} 应钉 {cpu} 号核");
        let fifo_idx = s0_args.iter().position(|a| a == "--serial-fifo").unwrap();
        assert_eq!(s0_args[fifo_idx + 1], "70", "{name}");
    }
}

/// **round 1 P1-4**：生产 S1 / gateway `WatchdogSec=5s`（Spec D12 拍板 +
/// PX-16 "测试模板 2s / 生产 5s"）；S0 arm unit 与 freeze 测试模板保持 2s。
#[test]
fn production_watchdog_values_are_five_seconds() {
    for (_, name) in S1_UNITS {
        assert_eq!(single(name, "WatchdogSec"), "5s", "{name}");
    }
    for (_, name, _) in GATEWAY_UNITS {
        assert_eq!(single(name, "WatchdogSec"), "5s", "{name}");
    }
    // 反向：测试模板不受影响，仍是秒级 2s。
    for tpl in [
        "xlerobot-s1-freeze-test.service",
        "xlerobot-gateway-freeze-test.service",
    ] {
        assert_eq!(single(tpl, "WatchdogSec"), "2s", "{tpl}: 测试模板不得改");
    }
}

#[test]
fn arm_units_declare_runtime_directory_triple() {
    for (_, name) in ARM_UNITS {
        assert_eq!(
            single(name, "RuntimeDirectory"),
            "robot-system/preflight",
            "{name}"
        );
        assert_eq!(single(name, "RuntimeDirectoryMode"), "0750", "{name}");
        assert_eq!(single(name, "RuntimeDirectoryPreserve"), "yes", "{name}");
    }
}

#[test]
fn arm_units_require_handoff_and_conflict_legacy() {
    for (_, name) in ARM_UNITS {
        assert_contains_all(name, "Requires", &[HANDOFF]);
        assert_contains_all(name, "After", &[HANDOFF]);
        assert_contains_all(name, "Conflicts", &LEGACY_UNITS);
        assert_contains_all(name, "After", &LEGACY_UNITS);
    }
}

// ============================================================================
// R10-1 三处 config 路径一致锁
// ============================================================================

/// 从 arm unit 的 `ExecStart` 还原一个 [`GuardConfig`]（`--` 之后原样带上）。
fn guard_config_from_unit(name: &str, arm: DeployArm, robot_id: &str) -> GuardConfig {
    let argv = exec_start_argv(name);
    GuardConfig {
        arm,
        config_path: PathBuf::from(opt(&argv, "--config")),
        s0_bin: PathBuf::from(opt(&argv, "--s0-bin")),
        runtime_dir: PathBuf::from("/run/robot-system/preflight"),
        robot_id: robot_id.to_string(),
        s0_args: s0_args_of(&argv).into_iter().map(OsString::from).collect(),
    }
}

#[test]
fn s0_args_must_not_carry_config_in_the_unit() {
    for (_, name) in ARM_UNITS {
        let argv = exec_start_argv(name);
        let s0_args = s0_args_of(&argv);
        assert!(
            !s0_args
                .iter()
                .any(|a| a == "--config" || a.starts_with("--config=")),
            "{name}: `--` 之后不得自带 --config（guard 自动追加）"
        );
    }
}

/// handoff 的 `--left/right-config`、arm guard 的 `--config`、guard **自动
/// 追加**给 `s0_daemon` 的 `--config` —— 三处必须是同一绝对路径。
#[test]
fn three_config_references_share_one_literal() {
    let handoff = exec_start_argv(HANDOFF);
    for (side, name, arm, robot_id) in [
        ("left", ARM_UNITS[0].1, DeployArm::Left, "xlerobot_left"),
        ("right", ARM_UNITS[1].1, DeployArm::Right, "xlerobot_right"),
    ] {
        let expected = if side == "left" {
            opt(&handoff, "--left-config")
        } else {
            opt(&handoff, "--right-config")
        };
        let cfg = guard_config_from_unit(name, arm, robot_id);
        // ② arm guard 的 --config
        assert_eq!(
            cfg.config_path,
            PathBuf::from(&expected),
            "{name}: guard --config 必须 == handoff 同侧 config"
        );
        // ③ guard 自动追加给 s0_daemon 的 --config
        let args = build_s0_args(&cfg).expect("unit 的 s0 args 不含 --config");
        let appended: Vec<_> = args
            .iter()
            .enumerate()
            .filter(|(_, a)| *a == &OsString::from("--config"))
            .collect();
        assert_eq!(appended.len(), 1, "{name}: --config 只能出现一次");
        let idx = appended[0].0;
        assert_eq!(
            args[idx + 1],
            OsString::from(&expected),
            "{name}: 追加值必须 == 同一绝对路径"
        );
    }
}

/// 拒绝 case：`--` 之后带 `--config` → 配置错误退 86。
#[test]
fn duplicate_config_from_unit_args_exits_86() {
    let cfg = guard_config_from_unit(ARM_UNITS[0].1, DeployArm::Left, "xlerobot_left");
    for extra in [
        vec![
            OsString::from("--config"),
            OsString::from("/etc/robot/other.toml"),
        ],
        vec![OsString::from("--config=/etc/robot/other.toml")],
    ] {
        let mut polluted = cfg.clone();
        polluted.s0_args.extend(extra.clone());
        let err = build_s0_args(&polluted).expect_err("必须拒绝");
        assert_eq!(err, GuardExit::ExecFailed, "{extra:?}");
        assert_eq!(err.code(), GUARD_STOP_EXIT_CODE);
    }
}

/// latch 单源：guard provision 的路径 == unit 里给 `s0_daemon` 的
/// `--latch-path`，且与 D2 身份表逐字一致。
#[test]
fn latch_path_follows_the_unit_argv() {
    for (name, arm, robot_id) in [
        (ARM_UNITS[0].1, DeployArm::Left, "xlerobot_left"),
        (ARM_UNITS[1].1, DeployArm::Right, "xlerobot_right"),
    ] {
        let cfg = guard_config_from_unit(name, arm, robot_id);
        assert_eq!(
            cfg.resolve_latch_path().expect("unit argv 必须单源可解析"),
            PathBuf::from(format!("/var/lib/robot/{robot_id}/estop_latch")),
            "{name}"
        );
    }
}

// ============================================================================
// S1 / gateway 依赖图（r5 V4）
// ============================================================================

#[test]
fn s1_units_hard_depend_on_same_arm_s0() {
    for ((side, name), (_, s0_name)) in S1_UNITS.into_iter().zip(ARM_UNITS) {
        assert_contains_all(name, "Requires", &[s0_name]);
        assert_contains_all(name, "After", &[s0_name]);
        assert_eq!(single(name, "Type"), "notify", "{name}");
        assert_eq!(single(name, "WatchdogSec"), "5s", "{name}");
        let argv = exec_start_argv(name);
        assert!(argv[0].ends_with("s1_daemon"), "{name}: {argv:?}");
        assert_eq!(opt(&argv, "--robot-id"), format!("xlerobot_{side}"));
        assert_eq!(opt(&argv, "--hardware-id"), format!("{side}_arm"));
        assert_eq!(
            opt(&argv, "--config"),
            format!("/etc/robot/xlerobot_{side}_arm.toml")
        );
    }
}

#[test]
fn gateway_units_require_s0_and_want_s1() {
    for ((side, name, listen), ((_, s0_name), (_, s1_name))) in GATEWAY_UNITS
        .into_iter()
        .zip(ARM_UNITS.into_iter().zip(S1_UNITS))
    {
        // S0 = 硬依赖（state 池 + estop 链）。
        assert_contains_all(name, "Requires", &[s0_name]);
        assert_contains_all(name, "After", &[s0_name]);
        // S1 = Wants（自动拉起，但 S1 重启不连带停 gateway）。
        assert_contains_all(name, "Wants", &[s1_name]);
        assert_contains_all(name, "After", &[s1_name]);
        assert!(
            !directives(name, "Requires").join(" ").contains(s1_name),
            "{name}: S1 不得是 Requires（会连带停 gateway）"
        );
        let argv = exec_start_argv(name);
        assert!(argv[0].ends_with("robot_rpc_gateway"), "{name}: {argv:?}");
        assert_eq!(opt(&argv, "--listen-addr"), listen, "{name}");
        assert_eq!(opt(&argv, "--robot-id"), format!("xlerobot_{side}"));
        assert_eq!(
            opt(&argv, "--config"),
            format!("/etc/robot/xlerobot_{side}_arm.toml")
        );
        assert_eq!(opt(&argv, "--rpc-roles-toml"), "/etc/robot/rpc_roles.toml");
    }
}
