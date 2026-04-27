//! `ClearEStop` control action handler。
//!
//! `docs/00_design.md` §6.6.1（2026-04-27 errata）规定的硬顺序：
//!
//! ```text
//! TorqueOffConfirmed gate
//!     → save_estop_latch(false)               // primary + bak fsync
//!     → executor_ctrl.clear_safety_latch()
//!     → guards.estop_latched = false
//!                                             → 返回 Ack
//! ```
//!
//! 本模块实现 `try_apply_clear_estop`。三步顺序故意写成连续四行可见，便于
//! reviewer 一眼看完；任何中间步失败，**后续步骤一律不执行**，调用方也
//! **不**得 commit_transition / set_policy（state 留 Safe，下次 tick 由
//! 上层重新触发 ClearEStop event 重试）。
//!
//! ### 为什么不传 `metrics`
//!
//! reviewer A1-2 拍板：本 action handler 只负责"顺序与失败不变性"；计数由
//! `ipc_tick` / `ipc_thread` 调用方根据返回 `ClearEStopOutcome` /
//! `ClearEStopError` 自行决定。避免把 ipc_thread 的 metrics 类型耦合进
//! action 模块。
//!
//! ### 为什么 gate 看 `executor_ctrl.safety_state()` 而不是 `guards.estop_latched`
//!
//! `safety_state` 是硬件 / SerialRT 实际断扭确认（`TorqueOffConfirmed`），
//! `guards.estop_latched` 是软件副本。emergency 触发瞬间 client 端
//! `safety_stop()` 会把 `safety_state` 立刻重置为 `StopCommanded`，rt /
//! serial 端确认真断扭后才升回 `TorqueOffConfirmed`。看 `safety_state`
//! 才能挡住"motor 还在转就 clear latch"这类 race。

#![allow(clippy::doc_markdown)]

use std::io;
use std::path::Path;

use robot_core::executor_ctrl::{ExecutorCtrlClient, OutputSafetyState};
use robot_core::time::RobotClock;

use crate::estop_latch::save_estop_latch_clear;
use crate::state_machine::{Guards, S0State};

/// 成功路径返回值。
///
/// `cleared_at_ns` = 三步全部成功后的 `clock.now()` mono raw 时间戳；
/// A1-3 RR ack 字段 `cleared_at_ns` 直接复用。A1-2 阶段 ipc_tick / ipc_thread
/// 还不读这个字段，故标 `#[allow(dead_code)]` 保留口子。
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)] // A1-3 接 robot-rpc RR ack 时复用
pub(crate) struct ClearEStopOutcome {
    pub(crate) cleared_at_ns: u64,
}

/// 失败原因。仅两类（reviewer A1-2 拍板）：
///
/// - `NotTorqueOffConfirmed`：硬件断扭未确认，gate 拒。
/// - `FsyncFailed`：latch primary / bak 任一写盘失败（包含 path 不存在 /
///   权限不足等所有 IO 错）。
///
/// 携带的 `actual` / `io::Error` 字段 A1-2 阶段 ipc_tick 不消费（只 match
/// variant 决定是否计 `latch_write_errors`）；A1-3 接 RR error mapping
/// 时会读，故标 `#[allow(dead_code)]` 保留口子。
#[derive(Debug)]
#[allow(dead_code)] // A1-3 接 robot-rpc RR error mapping 时复用
pub(crate) enum ClearEStopError {
    NotTorqueOffConfirmed { actual: OutputSafetyState },
    FsyncFailed(io::Error),
}

/// 执行 `ClearEStop` control action。
///
/// 调用前置条件（caller 责任）：
///
/// - `*state == S0State::Safe`（由 `plan_transition(Safe, ClearEStop) =>
///   Ok(Safe)` 守门）。非 Safe state 不得调用本函数。
/// - `latch_path` 必须可写（生产期 strict policy 已保证；mock / 单测可用
///   `tempdir()`）。
///
/// 返回 `Ok(ClearEStopOutcome)` 的语义保证：
///
/// 1. `executor_ctrl.safety_state()` 在调用瞬间 == `TorqueOffConfirmed`；
/// 2. `latch_path` primary + bak 已被 fsync 写为 `0\n`；
/// 3. `executor_ctrl.clear_safety_latch()` 已被调用一次；
/// 4. `guards.estop_latched` 已被翻为 `false`。
///
/// 任一步失败 → 返 `Err(_)`，**后续步骤不执行**，`guards` 不动、`executor`
/// 不调 clear、磁盘 latch 不动。
pub(crate) fn try_apply_clear_estop(
    guards: &mut Guards,
    executor_ctrl: &dyn ExecutorCtrlClient,
    latch_path: &Path,
    clock: &dyn RobotClock,
) -> Result<ClearEStopOutcome, ClearEStopError> {
    // 1. TorqueOffConfirmed gate
    let actual = executor_ctrl.safety_state();
    if actual != OutputSafetyState::TorqueOffConfirmed {
        return Err(ClearEStopError::NotTorqueOffConfirmed { actual });
    }

    // 2. fsync latch=false（backup-first，详 estop_latch::save_estop_latch_clear）：
    //    partial failure 时保证 primary 不动 → 重启 load 仍读到 latched=true，
    //    不破坏 §6.7 #4 "S0 重启不失忆" + A1-2 失败不变量"磁盘 latch 不动"。
    save_estop_latch_clear(latch_path).map_err(ClearEStopError::FsyncFailed)?;

    // 3. executor latch（必须在 fsync 持久化之后；fsync 失败时本步必须不执行）
    executor_ctrl.clear_safety_latch();

    // 4. guards 内存副本翻转（必须最后做）
    guards.estop_latched = false;

    Ok(ClearEStopOutcome {
        cleared_at_ns: clock.now().0,
    })
}

// ============================================================================
// dry_run evaluator (A1-3 (1/3a) reviewer follow-up 2 拍板)
// ============================================================================

/// dry_run 评估结果（reviewer A1-3 拍板用 enum，不用 `Result`）：
///
/// - `WouldClear`：state == Safe + safety_state == TorqueOffConfirmed →
///   "if you re-send a live ClearEStop now, it would clear latch"。
/// - `StateNotSafe` / `NotTorqueOffConfirmed`：domain `FailedPrecondition` —
///   gate 拒，由 ipc_thread / gateway 显式 match 到 RR / gRPC status。
///
/// dry_run **不**走 fsync，因此本枚举**不**含 `FsyncFailed` variant。
/// `would_clear_at_ns` = `clock.now()` 取的 mono raw 时间戳，仅作为
/// audit / response 字段（不代表硬件真清的时刻——dry_run 不动硬件）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)] // A1-3 (1/3b) ipc_thread RR drain 接通时消费
pub(crate) enum ClearEStopDryRunOutcome {
    WouldClear { would_clear_at_ns: u64 },
    StateNotSafe { actual: S0State },
    NotTorqueOffConfirmed { actual: OutputSafetyState },
}

/// dry_run 评估：read-only 检查 ClearEStop 在当前 state / safety_state /
/// guards 下是否会成功，**不**触动任何 in-memory / 磁盘 / executor 状态。
///
/// reviewer A1-3 follow-up 2 拍板：
///
/// - dry_run 必须进 daemon/S0 action layer，**不**能 gateway 短路（gateway
///   没有权威状态）。
/// - 不 push `Event::ClearEStop`、不调 [`try_apply_clear_estop`]、不碰
///   latch / executor / guard。
/// - 必要时可参考 `guards.estop_latched` 生成更清楚的 response 文案
///   （由调用方 ipc_thread / gateway 做），但 gate 仍以
///   `executor_ctrl.safety_state() == TorqueOffConfirmed` 为权威——
///   不能靠 guard 副本绕过 torque gate。
///
/// 调用方契约：本函数只读，可在 `ipc_thread` 主循环 RR drain 阶段直接
/// 同步调用，不进 events queue / `ipc_tick`。
#[allow(dead_code)] // A1-3 (1/3b) ipc_thread RR drain 接通时消费
pub(crate) fn evaluate_clear_estop_dry_run(
    state: S0State,
    guards: &Guards,
    executor_ctrl: &dyn ExecutorCtrlClient,
    clock: &dyn RobotClock,
) -> ClearEStopDryRunOutcome {
    // 1) state machine gate：state != Safe → live ClearEStop 也会被
    //    `plan_transition(*, ClearEStop)` 返 None continue，dry_run 直接
    //    返 StateNotSafe。
    if state != S0State::Safe {
        return ClearEStopDryRunOutcome::StateNotSafe { actual: state };
    }

    // 2) hardware torque gate：权威看 executor_ctrl.safety_state()，不看
    //    guards.estop_latched 副本（reviewer 拍板：不能靠 guard 绕过
    //    torque gate）。
    let actual = executor_ctrl.safety_state();
    if actual != OutputSafetyState::TorqueOffConfirmed {
        return ClearEStopDryRunOutcome::NotTorqueOffConfirmed { actual };
    }

    // 3) WouldClear: 两条 gate 都过 → live ClearEStop 此刻会清 latch。
    //    `guards.estop_latched` 当前值由调用方 ipc_thread 自行参考，本函数
    //    不携带（保持 read-only 最小返回面）。
    let _ = guards; // 显式标记未消费（reviewer 拍板留口给调用方做 response 文案）
    ClearEStopDryRunOutcome::WouldClear {
        would_clear_at_ns: clock.now().0,
    }
}

// ============================================================================
// Tests —— A1-2 reviewer 拍板锁三条失败路径 + 成功硬顺序
//          A1-3 (1/3a) 加 dry_run evaluator 三条路径锁测
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::Ordering;

    use robot_core::adapter::ControlMode;
    use robot_core::executor_ctrl::{
        AdapterNormalReq, AdapterResp, ExecutorCtrlClient, NormalReqStatus, OutputPolicy,
        OutputSafetyState,
    };
    use robot_core::time::{MockClock, MonoRawNs};
    use tempfile::tempdir;

    use crate::estop_latch::{load_estop_latch, save_estop_latch};
    use crate::hard_rt_ctrl::{HardRtCtrlClient, HardRtCtrlInner};
    use crate::state_machine::{EnabledSub, Guards};

    fn healthy_guards_latched() -> Guards {
        Guards {
            estop_latched: true, // EStop 已 latch
            adapter_connected: true,
            calibration_loaded: true,
            joint_state_valid: true,
            power_ok: true,
            heartbeat_ok: true,
            cmd_deadline_valid: true,
            limits_valid: true,
            hw_enable_ok: true,
            adapter_fault_cleared: true,
            command_mode: ControlMode::Position,
            on_timeout_hint: EnabledSub::CtrlLostHold,
        }
    }

    /// 设置真实 `HardRtCtrlInner` 的 `safety_state` + `safety_latched` 到指定值。
    fn make_executor(safety_state: OutputSafetyState, latched: bool) -> Arc<HardRtCtrlInner> {
        let inner = Arc::new(HardRtCtrlInner::new());
        inner
            .safety_state
            .store(safety_state.to_u8(), Ordering::Release);
        inner.safety_latched.store(latched, Ordering::Release);
        inner
    }

    // ────────────────────────────────────────────────────────────────────────
    // 1) gate 拒：safety_state != TorqueOffConfirmed
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn clear_estop_blocked_when_safety_state_not_torque_off_confirmed() {
        let tmp = tempdir().unwrap();
        let latch_path = tmp.path().join("estop_latch");
        save_estop_latch(&latch_path, true).unwrap(); // 预 provision latch=true
        let bak_path = tmp.path().join("estop_latch.bak");

        // safety_state = TorqueOn (即电机仍在使能态)
        let inner = make_executor(OutputSafetyState::TorqueOn, true);
        let client = HardRtCtrlClient::new(Arc::clone(&inner));

        let mut guards = healthy_guards_latched();
        let clock = MockClock::new(MonoRawNs::ZERO);

        let res = try_apply_clear_estop(&mut guards, &client, &latch_path, &clock);

        // action 必须拒
        match res {
            Err(ClearEStopError::NotTorqueOffConfirmed { actual }) => {
                assert_eq!(actual, OutputSafetyState::TorqueOn);
            }
            other => panic!("expected NotTorqueOffConfirmed, got {other:?}"),
        }

        // 副作用全不变
        assert!(guards.estop_latched, "guard 不得翻");
        assert!(
            inner.safety_latched.load(Ordering::Acquire),
            "executor.safety_latched 不得清"
        );
        // latch 文件保持原值（"1\n"）
        assert_eq!(std::fs::read_to_string(&latch_path).unwrap(), "1\n");
        assert_eq!(std::fs::read_to_string(&bak_path).unwrap(), "1\n");
    }

    // ────────────────────────────────────────────────────────────────────────
    // 2) fsync 失败：latch_path 不可写
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn clear_estop_blocked_when_fsync_fails() {
        // 不可写：指向不存在的目录
        let bad_path = std::path::PathBuf::from("/proc/nonexistent/estop_latch");

        let inner = make_executor(OutputSafetyState::TorqueOffConfirmed, true);
        let client = HardRtCtrlClient::new(Arc::clone(&inner));

        let mut guards = healthy_guards_latched();
        let clock = MockClock::new(MonoRawNs::ZERO);

        let res = try_apply_clear_estop(&mut guards, &client, &bad_path, &clock);

        // action 必须拒
        match res {
            Err(ClearEStopError::FsyncFailed(_)) => {}
            other => panic!("expected FsyncFailed, got {other:?}"),
        }

        // 副作用全不变
        assert!(guards.estop_latched, "guard 不得翻");
        assert!(
            inner.safety_latched.load(Ordering::Acquire),
            "executor.safety_latched 不得清（fsync 失败时绝不能调）"
        );
    }

    // ────────────────────────────────────────────────────────────────────────
    // 3) 成功硬顺序：fsync → clear_safety_latch → guard
    //
    //    用一个 spy executor，在 `clear_safety_latch` 调用瞬间读 latch 文件
    //    内容快照。
    //    - 若 fsync 在 clear_safety_latch 之前完成（正确）：spy 看到 "0\n"
    //    - 若 fsync 在 clear_safety_latch 之后（错误）：spy 看到 "1\n"
    //    （第三步 guard 翻转在 clear_safety_latch 之后是源码 invariant，
    //     在三行连续代码内可一眼 review 锁住）
    // ────────────────────────────────────────────────────────────────────────

    /// spy executor：包真实 `HardRtCtrlInner` + 在 `clear_safety_latch` 调用
    /// 瞬间读 latch 文件内容，把快照存到 `latch_at_clear`。用于 strict
    /// ordering 测试观察"fsync 是否在 clear_safety_latch 之前完成"。
    struct Spy {
        inner: Arc<HardRtCtrlInner>,
        latch_path: std::path::PathBuf,
        latch_at_clear: Mutex<Option<String>>,
    }

    impl ExecutorCtrlClient for Spy {
        fn set_policy(&self, p: OutputPolicy) {
            self.inner.policy.store(p.to_u8(), Ordering::Release);
        }
        fn safety_stop(&self) {
            // 不会被调（成功路径）
        }
        fn request_normal(&self, _req: AdapterNormalReq) -> NormalReqStatus {
            NormalReqStatus::Busy
        }
        fn poll_last_normal_resp(&self, _seq: u64) -> Option<AdapterResp> {
            None
        }
        fn clear_safety_latch(&self) {
            // **关键观察**：在 atomic store 之前读 latch 文件，证明 fsync 已完成
            let content = std::fs::read_to_string(&self.latch_path).unwrap_or_default();
            *self.latch_at_clear.lock().unwrap() = Some(content);
            self.inner.safety_latched.store(false, Ordering::Release);
        }
        fn safety_state(&self) -> OutputSafetyState {
            OutputSafetyState::from_u8(self.inner.safety_state.load(Ordering::Acquire))
        }
    }

    #[test]
    fn clear_estop_success_path_strict_ordering() {
        let tmp = tempdir().unwrap();
        let latch_path = tmp.path().join("estop_latch");
        save_estop_latch(&latch_path, true).unwrap(); // 预 provision latch=true
        let bak_path = tmp.path().join("estop_latch.bak");

        // safety_state = TorqueOffConfirmed（gate 通过条件）
        let inner = make_executor(OutputSafetyState::TorqueOffConfirmed, true);

        let spy = Spy {
            inner: Arc::clone(&inner),
            latch_path: latch_path.clone(),
            latch_at_clear: Mutex::new(None),
        };

        let mut guards = healthy_guards_latched();
        let clock = MockClock::new(MonoRawNs(12_345));

        let res = try_apply_clear_estop(&mut guards, &spy, &latch_path, &clock);

        // 1) action 返 Ok，cleared_at_ns 来自 clock.now()
        let outcome = res.expect("成功路径");
        assert_eq!(outcome.cleared_at_ns, 12_345);

        // 2) clear_safety_latch 调用瞬间，latch 文件内容已经是 "0\n"
        //    （这等价于 "fsync 在 clear_safety_latch 之前完成"）
        assert_eq!(
            spy.latch_at_clear.lock().unwrap().as_deref(),
            Some("0\n"),
            "fsync 必须先于 clear_safety_latch 完成"
        );

        // 3) guard 已翻
        assert!(!guards.estop_latched);

        // 4) executor.safety_latched 已清
        assert!(!inner.safety_latched.load(Ordering::Acquire));

        // 5) latch 文件最终内容（primary + bak 都是 "0\n"）
        assert_eq!(std::fs::read_to_string(&latch_path).unwrap(), "0\n");
        assert_eq!(std::fs::read_to_string(&bak_path).unwrap(), "0\n");
    }

    // ────────────────────────────────────────────────────────────────────────
    // 4) partial failure 锁测（A1-2 reviewer round 2）：try_apply_clear_estop
    //    在 fsync partial failure 时磁盘 latch 仍 latched=true，guard /
    //    executor 全不动；重启 daemon 不会被错误清。
    //
    //    旧实现复用 save_estop_latch(_, false) (primary-first) 时这条路径
    //    会暴露 bug：primary 已写 "0\n"，bak 失败返 Err，但 daemon
    //    crash/restart 后 load_estop_latch 走 primary 读 "0\n" →
    //    latched=false → 软件 latch 被错误清掉。本测试钉死 backup-first
    //    save_estop_latch_clear 修复后，partial failure 不再破坏 §6.7 #4。
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn clear_estop_partial_backup_failure_preserves_disk_latched_true() {
        let tmp = tempdir().unwrap();
        let latch_path = tmp.path().join("estop_latch");
        let bak_path = tmp.path().join("estop_latch.bak");

        // 预置 primary + bak = "1\n"（模拟 EStop 已落 latch + ipc_thread
        // 通用 hook 已 fsync）
        save_estop_latch(&latch_path, true).unwrap();

        // 把 bak 替换成非空目录（让 backup-first save_single_atomic 第一步失败）
        std::fs::remove_file(&bak_path).unwrap();
        std::fs::create_dir(&bak_path).unwrap();
        std::fs::write(bak_path.join("nonempty"), b"x").unwrap();

        let inner = make_executor(OutputSafetyState::TorqueOffConfirmed, true);
        let client = HardRtCtrlClient::new(Arc::clone(&inner));

        let mut guards = healthy_guards_latched();
        let clock = MockClock::new(MonoRawNs::ZERO);

        let res = try_apply_clear_estop(&mut guards, &client, &latch_path, &clock);

        // action 必须返 FsyncFailed
        match res {
            Err(ClearEStopError::FsyncFailed(_)) => {}
            other => panic!("expected FsyncFailed, got {other:?}"),
        }

        // guard / executor 不动
        assert!(guards.estop_latched, "guard 不得翻");
        assert!(
            inner.safety_latched.load(Ordering::Acquire),
            "executor.safety_latched 不得清"
        );

        // **核心断言**：primary 仍 "1\n"（backup-first 保证 partial 时 primary 不动）
        assert_eq!(
            std::fs::read(&latch_path).unwrap(),
            b"1\n",
            "primary 必须保持 latched=true"
        );

        // **核心断言**：daemon crash/restart 后 load 仍返 latched=true
        // （§6.7 #4 "S0 重启不失忆" + A1-2 失败不变量"磁盘 latch 不动"）
        assert!(
            load_estop_latch(&latch_path).unwrap(),
            "load_estop_latch 必须仍返 latched=true；如果失败说明 backup-first 修复回归了"
        );
    }

    // ────────────────────────────────────────────────────────────────────────
    // 5) dry_run evaluator (A1-3 (1/3a) reviewer follow-up 2 拍板锁三条)
    //
    //    硬边界：dry_run 必须进 daemon/S0 action layer，gateway 没有权威状态
    //    不能短路。read-only 检查 state == Safe + safety_state ==
    //    TorqueOffConfirmed；任何路径都**不**碰 latch 文件 / executor latch /
    //    guard。
    // ────────────────────────────────────────────────────────────────────────

    /// **A1-3 (1/3a)**：dry_run gate 全过 → 返 `WouldClear`，副作用全不动。
    ///
    /// `state == Safe` & `safety_state == TorqueOffConfirmed` → 评估返
    /// `WouldClear { would_clear_at_ns }`。预置 latch primary / bak `= "1\n"`、
    /// `executor.safety_latched = true`、`guards.estop_latched = true`，
    /// 评估完后**全部**保持原值。
    #[test]
    fn dry_run_gate_pass_returns_would_clear_no_side_effects() {
        let tmp = tempdir().unwrap();
        let latch_path = tmp.path().join("estop_latch");
        let bak_path = tmp.path().join("estop_latch.bak");
        save_estop_latch(&latch_path, true).unwrap(); // 预 provision latch=true

        let inner = make_executor(OutputSafetyState::TorqueOffConfirmed, true);
        let client = HardRtCtrlClient::new(Arc::clone(&inner));

        let guards = healthy_guards_latched();
        let clock = MockClock::new(MonoRawNs(98_765));

        let res = evaluate_clear_estop_dry_run(S0State::Safe, &guards, &client, &clock);

        // 1) 评估返 WouldClear，would_clear_at_ns == clock.now()
        match res {
            ClearEStopDryRunOutcome::WouldClear { would_clear_at_ns } => {
                assert_eq!(would_clear_at_ns, 98_765);
            }
            other => panic!("expected WouldClear, got {other:?}"),
        }

        // 2) 副作用全不动：guard / executor / latch 全保持原值
        assert!(guards.estop_latched, "guard 不得翻（dry_run 不碰 guard）");
        assert!(
            inner.safety_latched.load(Ordering::Acquire),
            "executor.safety_latched 不得清（dry_run 不调 clear_safety_latch）"
        );
        assert_eq!(
            std::fs::read(&latch_path).unwrap(),
            b"1\n",
            "latch primary 不得被写（dry_run 不调 fsync）"
        );
        assert_eq!(
            std::fs::read(&bak_path).unwrap(),
            b"1\n",
            "latch backup 不得被写"
        );
    }

    /// **A1-3 (1/3a)**：dry_run state != Safe → 返 `StateNotSafe`，
    /// 副作用全不动。
    #[test]
    fn dry_run_state_not_safe_returns_state_not_safe_no_side_effects() {
        let tmp = tempdir().unwrap();
        let latch_path = tmp.path().join("estop_latch");
        save_estop_latch(&latch_path, true).unwrap();

        // safety_state = TorqueOffConfirmed（torque gate 通过），但 state = Ready
        let inner = make_executor(OutputSafetyState::TorqueOffConfirmed, true);
        let client = HardRtCtrlClient::new(Arc::clone(&inner));

        let guards = healthy_guards_latched();
        let clock = MockClock::new(MonoRawNs::ZERO);

        let res = evaluate_clear_estop_dry_run(S0State::Ready, &guards, &client, &clock);

        // 1) 评估返 StateNotSafe { actual: Ready }
        match res {
            ClearEStopDryRunOutcome::StateNotSafe { actual } => {
                assert_eq!(actual, S0State::Ready);
            }
            other => panic!("expected StateNotSafe, got {other:?}"),
        }

        // 2) 副作用全不动
        assert!(guards.estop_latched);
        assert!(inner.safety_latched.load(Ordering::Acquire));
        assert_eq!(std::fs::read(&latch_path).unwrap(), b"1\n");
    }

    /// **A1-3 (1/3a)**：dry_run state == Safe 但 safety_state != TorqueOffConfirmed
    /// → 返 `NotTorqueOffConfirmed { actual }`，副作用全不动。
    ///
    /// reviewer 拍板：torque gate 看 `executor_ctrl.safety_state()` 权威，
    /// 不看 `guards.estop_latched` 副本——本测试预置 guard.estop_latched=false
    /// 但 safety_state=TorqueOn，仍必须返 NotTorqueOffConfirmed（不能靠
    /// guard 副本绕过 torque gate）。
    #[test]
    fn dry_run_safety_state_not_torque_off_confirmed_returns_not_torque_off_confirmed() {
        let tmp = tempdir().unwrap();
        let latch_path = tmp.path().join("estop_latch");
        save_estop_latch(&latch_path, true).unwrap();

        // state = Safe，但 safety_state = TorqueOn（电机仍 enabled）
        let inner = make_executor(OutputSafetyState::TorqueOn, true);
        let client = HardRtCtrlClient::new(Arc::clone(&inner));

        // 关键：guards.estop_latched = false（被人为清掉），看 dry_run 是否
        // 仍以 torque gate 为权威拒
        let mut guards = healthy_guards_latched();
        guards.estop_latched = false;

        let clock = MockClock::new(MonoRawNs::ZERO);

        let res = evaluate_clear_estop_dry_run(S0State::Safe, &guards, &client, &clock);

        // 1) 评估返 NotTorqueOffConfirmed { actual: TorqueOn }（不靠 guard 绕过）
        match res {
            ClearEStopDryRunOutcome::NotTorqueOffConfirmed { actual } => {
                assert_eq!(actual, OutputSafetyState::TorqueOn);
            }
            other => panic!(
                "expected NotTorqueOffConfirmed (torque gate 必须以 safety_state 为权威，\
                 不能靠 guards.estop_latched=false 绕过), got {other:?}"
            ),
        }

        // 2) 副作用全不动
        assert!(inner.safety_latched.load(Ordering::Acquire));
        assert_eq!(std::fs::read(&latch_path).unwrap(), b"1\n");
    }
}
