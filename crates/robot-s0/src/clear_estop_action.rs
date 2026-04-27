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

use crate::estop_latch::save_estop_latch;
use crate::state_machine::Guards;

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

    // 2. fsync(primary + bak) latch=false
    save_estop_latch(latch_path, false).map_err(ClearEStopError::FsyncFailed)?;

    // 3. executor latch（必须在 fsync 持久化之后；fsync 失败时本步必须不执行）
    executor_ctrl.clear_safety_latch();

    // 4. guards 内存副本翻转（必须最后做）
    guards.estop_latched = false;

    Ok(ClearEStopOutcome {
        cleared_at_ns: clock.now().0,
    })
}

// ============================================================================
// Tests —— A1-2 reviewer 拍板锁三条失败路径 + 成功硬顺序
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

    use crate::estop_latch::save_estop_latch;
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
}
