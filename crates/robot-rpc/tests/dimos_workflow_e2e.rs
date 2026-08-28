//! **双臂 workflow cross-process e2e**（v0.5 候选 E 段(7)，Spec P.2.8 +
//! AC-E2/E4/E6/E7/E9/E11/E14/E15/E18）。
//!
//! ## 拓扑：三真同堂 ×2（D9 首例）
//!
//! 每个 case 由 `DualCtx` 起 **2×(s0 + s1 + gateway) = 6 子进程**（PD-E3：
//! `dimos_e2e_<pid>_<seq>_{left,right}` / `left_arm`・`right_arm` / 两端口
//! `127.0.0.1:0` 且断言不同 / 单 test `GLOBAL_LOCK` 串行 / RAII 清理 +
//! forensic dump）。协议编排全部走 lib 的
//! `robot_rpc::client::dual_arm_workflow`（e2e 与真机验收 bin **同向消费**
//! 同一份语义，禁止各自复制）。
//!
//! ## 运行前置（P.0 活清单）
//!
//! ```bash
//! cargo build --release --workspace
//! # AC-E14 执行期 failure / AC-E18 failure 分支需要 test-only fault backend：
//! cargo build --release -p robot-s1 --bin s1_daemon --features test-fault-backend
//! ```
//!
//! 漏编 `test-fault-backend` 时，仅 `*_failure` / `pair_partial_failure_paths`
//! 两个 case 在 bring-up 期失败（s1 `validate_profile_section` →
//! `UnknownBackend` → exit 3），其余 case 用
//! [`DualCtxOptions::scripted_only`] 不受影响。
//!
//! ## case ↔ AC 映射
//!
//! | case | AC |
//! |---|---|
//! | `read_state_both_arms_layout_validated` | E4 / E11-M1M2 |
//! | `chunk_pair_scripted_with_gripper` | E11-M3+M4（D6 同车）/ E15 终点一致 + 单调 |
//! | `consecutive_chunks_no_pullback_no_jump` | E7（G1 闭合） |
//! | `post_success_reset_matrix_{cancel,failure,ik,hold_pose}` | E18（RB5：**各分支独立 ctx**） |
//! | `start_pose_stale_rejected` | E6（SIGSTOP 真 s0 + 等 `max_stale_ms`） |
//! | `pair_partial_failure_paths` | E14（提交期 + 执行期） |
//! | `nontarget_fill_current` | E9（D8 缺位填当前位 + 漂移容差） |
//! | `s0_drops_wrong_layout_commands` | E2 跨进程（**独立 mini 拓扑** + raw 注入器） |

#![allow(clippy::doc_markdown)]

mod common;

use std::time::Duration;

use tonic::transport::Channel;
use uuid::Uuid;

use robot_rpc::client::dual_arm_workflow::{
    ActionKindWire, ActionSpec, ArmEndpoint, ArmPair, NO_PAYLOAD_SCHEMA_ID, PairConfig, PairError,
    PairResult, Side, SubmitFailure, TerminalState, run_chunk_pair,
};
use robot_rpc::client::layout::{HardwareLayout, load_hardware_layout, validate_state_layout};
use robot_rpc::client::trajectory::{
    TrajectoryInput, TrajectoryPoint, build_scripted_payload, to_scripted_waypoints,
};
use robot_rpc::proto::robot_control_client::RobotControlClient;
use robot_rpc::proto::{GetStateRequest, RobotState};

use common::client::{MAINTAINER_TOKEN, build_clear_error_command};
use common::dual_ctx::{ArmStack, DualCtx, DualCtxOptions, JointsSpec, global_lock};
use common::pair_endpoint::TonicArm;
use common::raw_s0;

// ============================================================================
// 常量 / 小工具
// ============================================================================

/// chunk 形态（D5：一次推理 = 10 waypoint × 100 ms，4 KiB gate 内）。
const CHUNK_STEPS: usize = 10;
const SEG_MS: u32 = 100;
/// mock adapter 的位置 echo 是 f32；1e-2 rad 容差远大于精度损失，也覆盖
/// 读写间隙漂移（AC-E9 定值）。
const POS_TOL: f64 = 1e-2;
/// 命令上线 + state 回传的稳定等待（50 Hz 链路，留足余量）。
const SETTLE: Duration = Duration::from_millis(400);
/// `[s1].max_stale_ms` 默认 200 ms；停 s0 后等这么久必然 stale。
const STALE_WAIT: Duration = Duration::from_millis(600);

/// e2e 用的协议窗口：比 lib 默认短，让失败路径快速收敛（chunk 只有 1 s）。
fn pair_cfg() -> PairConfig {
    let cfg = PairConfig {
        terminal_timeout: Duration::from_secs(8),
        ..PairConfig::default()
    };
    cfg.validate().expect("e2e PairConfig 必须自洽");
    cfg
}

fn arm_pair(ctx: &DualCtx) -> ArmPair<TonicArm> {
    ArmPair {
        left: TonicArm::new(Side::Left, ctx.left.channel.clone()),
        right: TonicArm::new(Side::Right, ctx.right.channel.clone()),
    }
}

/// `GetState`（读面 = M1/M2 的载体）。
async fn get_state(channel: &Channel, hardware_id: &str) -> RobotState {
    let mut client = RobotControlClient::new(channel.clone());
    client
        .get_state(GetStateRequest {
            hardware_id: hardware_id.to_string(),
        })
        .await
        .expect("GetState 必须成功")
        .into_inner()
}

/// 当前全向量位置（rad）。
async fn positions(arm: &ArmStack) -> Vec<f64> {
    get_state(&arm.channel, arm.hardware_id).await.position_rad
}

fn layout_of(arm: &ArmStack) -> HardwareLayout {
    load_hardware_layout(&arm.config_path, arm.hardware_id).expect("config 派生 layout")
}

fn max_abs_diff(a: &[f64], b: &[f64]) -> f64 {
    assert_eq!(a.len(), b.len(), "向量长度必须一致");
    a.iter()
        .zip(b)
        .map(|(x, y)| (x - y).abs())
        .fold(0.0_f64, f64::max)
}

/// 构造一条 chunk 的 `ScriptedPayload`：只对 `moves` 里的关节下发目标，
/// 其余关节由转换器按 D8 **填当前位**。
///
/// `points[0]` 严格 t=0 且等于当前位（段(6) PX-13 契约）。
fn chunk_payload(layout: &HardwareLayout, current: &[f64], moves: &[(usize, f64)]) -> Vec<u8> {
    assert!(!moves.is_empty(), "至少动一个关节");
    let joint_names: Vec<String> = moves
        .iter()
        .map(|(i, _)| layout.joints[*i].clone())
        .collect();
    let start: Vec<f64> = moves.iter().map(|(i, _)| current[*i]).collect();
    let mut points = vec![TrajectoryPoint {
        time_from_start_s: 0.0,
        positions: start.clone(),
    }];
    for k in 1..=CHUNK_STEPS {
        #[allow(clippy::cast_precision_loss)]
        let ratio = k as f64 / CHUNK_STEPS as f64;
        points.push(TrajectoryPoint {
            #[allow(clippy::cast_precision_loss)]
            time_from_start_s: (k as f64) * f64::from(SEG_MS) / 1000.0,
            positions: moves
                .iter()
                .zip(&start)
                .map(|((_, target), s)| s + (target - s) * ratio)
                .collect(),
        });
    }
    let traj = TrajectoryInput {
        joint_names,
        points,
    };
    let wps = to_scripted_waypoints(&traj, layout, current).expect("转换必须通过");
    build_scripted_payload(&wps)
}

/// 期望的 chunk 终点全向量（未命名关节 = 当前位）。
fn expected_final(current: &[f64], moves: &[(usize, f64)]) -> Vec<f64> {
    let mut out = current.to_vec();
    for (i, t) in moves {
        out[*i] = *t;
    }
    out
}

/// 采样 `count` 次位置（间隔 `interval`），用于单调 / 连续性断言。
async fn sample_positions(arm: &ArmStack, count: usize, interval: Duration) -> Vec<Vec<f64>> {
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        out.push(positions(arm).await);
        tokio::time::sleep(interval).await;
    }
    out
}

/// 跑一对 chunk 并要求 pair success。
async fn run_success_pair(ctx: &DualCtx, pair: &mut ArmPair<TonicArm>, moves: &[(usize, f64)]) {
    let clock = robot_core::time::SystemClock::new();
    let cfg = pair_cfg();
    let (cur_left, cur_right) = (positions(&ctx.left).await, positions(&ctx.right).await);
    let (layout_left, layout_right) = (layout_of(&ctx.left), layout_of(&ctx.right));
    let run = run_chunk_pair(
        pair,
        ActionSpec::scripted(chunk_payload(&layout_left, &cur_left, moves)),
        ActionSpec::scripted(chunk_payload(&layout_right, &cur_right, moves)),
        &clock,
        &cfg,
    )
    .await
    .expect("pair 必须成功");
    assert!(run.result.is_pair_success(), "{:?}", run.result);
    // D5 第 1/7 条：Ack 偏差 gate（实测值记录进日志供收紧）。
    eprintln!("[e2e] ack_skew = {:?}", run.ack_skew);
}

// ============================================================================
// AC-E4 / E11-M1M2：双臂读状态 + layout 硬校验
// ============================================================================

#[tokio::test]
async fn read_state_both_arms_layout_validated() {
    let ctx = DualCtx::bring_up_with("read_state", DualCtxOptions::scripted_only())
        .await
        .expect("bring up");
    for side in [Side::Left, Side::Right] {
        let arm = ctx.arm(side);
        let layout = layout_of(arm);
        let state = get_state(&arm.channel, arm.hardware_id).await;

        assert_eq!(state.hardware_id, arm.hardware_id);
        assert_eq!(
            state.position_rad.len(),
            layout.joints.len(),
            "{side}: 读面向量长度 = 关节数（M1/M2 载体）"
        );
        // AC-E4：config 派生 layout 与 wire layout_id 必须一致。
        validate_state_layout(&layout, &state).expect("layout 必须匹配");
        assert_eq!(state.layout_id, arm.layout_id);

        // AC-E4 反向：错 layout **硬失败不降级**。
        let wrong = HardwareLayout {
            layout_id: layout.layout_id ^ 0x1,
            ..layout
        };
        let err = validate_state_layout(&wrong, &state).expect_err("错 layout 必须硬失败");
        assert_eq!(err.got, state.layout_id);
    }
    ctx.shutdown().await.expect("shutdown clean");
}

// ============================================================================
// AC-E11 (M3+M4) / AC-E15：夹爪同车 + 终点一致 + 轨迹单调
// ============================================================================

#[tokio::test]
async fn chunk_pair_scripted_with_gripper() {
    let ctx = DualCtx::bring_up_with("chunk_pair", DualCtxOptions::scripted_only())
        .await
        .expect("bring up");
    let gripper = ctx
        .options
        .joints
        .gripper_index()
        .expect("6 维形态必有 gripper");
    // 5 臂关节 + gripper 第 6 位**同车**（D6：0 新 method / 0 新 schema）。
    let moves = [(0_usize, 0.30_f64), (2, -0.20), (gripper, 0.50)];

    let mut pair = arm_pair(&ctx);
    let left_start = positions(&ctx.left).await;
    let expected = expected_final(&left_start, &moves);

    // 执行期采样（AC-E15 轨迹单调；夹爪从阶跃改成分段线性插值是**有意**
    // 语义变化，判据 = 终点一致 + 单调，不逐点比对阶跃波形）。
    let sample_chan = ctx.left.channel.clone();
    let hw = ctx.left.hardware_id;
    let sample_task = tokio::spawn(async move {
        let mut seq = Vec::new();
        for _ in 0..14 {
            seq.push(get_state(&sample_chan, hw).await.position_rad);
            tokio::time::sleep(Duration::from_millis(80)).await;
        }
        seq
    });

    run_success_pair(&ctx, &mut pair, &moves).await;
    let traj_samples = sample_task.await.expect("sample task");

    // 终点一致（两臂各自）。
    tokio::time::sleep(SETTLE).await;
    for side in [Side::Left, Side::Right] {
        let final_pos = positions(ctx.arm(side)).await;
        assert!(
            max_abs_diff(&final_pos, &expected) <= POS_TOL,
            "{side}: 终点必须等于末 waypoint，期望 {expected:?} 实得 {final_pos:?}"
        );
    }

    // 单调：gripper 目标是递增方向，采样序列不得回退（容差内）。
    let mut prev = f64::NEG_INFINITY;
    let mut gripper_moved = false;
    for s in &traj_samples {
        let v = s[gripper];
        assert!(
            v >= prev - POS_TOL,
            "gripper 轨迹必须单调不回退：{prev} → {v}（采样 = {traj_samples:?}）"
        );
        if v > left_start[gripper] + POS_TOL {
            gripper_moved = true;
        }
        prev = v;
    }
    assert!(gripper_moved, "采样期内 gripper 必须真的动过");
    ctx.shutdown().await.expect("shutdown clean");
}

// ============================================================================
// AC-E7：连续两 chunk 无回拉、无跳变（G1 闭合）
// ============================================================================

#[tokio::test]
async fn consecutive_chunks_no_pullback_no_jump() {
    let ctx = DualCtx::bring_up_with("consecutive", DualCtxOptions::scripted_only())
        .await
        .expect("bring up");
    let mut pair = arm_pair(&ctx);
    let anchor = positions(&ctx.left).await;

    // chunk A。
    let moves_a = [(0_usize, 0.25_f64), (1, 0.15)];
    run_success_pair(&ctx, &mut pair, &moves_a).await;
    tokio::time::sleep(SETTLE).await;
    let after_a = positions(&ctx.left).await;
    let expect_a = expected_final(&anchor, &moves_a);
    assert!(
        max_abs_diff(&after_a, &expect_a) <= POS_TOL,
        "chunk A 终点 {expect_a:?} vs {after_a:?}"
    );

    // **无回拉**：chunk 间隙 hold 在末位，不回启动 anchor（G1）。
    let gap = sample_positions(&ctx.left, 6, Duration::from_millis(80)).await;
    for s in &gap {
        assert!(
            max_abs_diff(s, &after_a) <= POS_TOL,
            "chunk 间隙必须 hold 在末位（G1 无回拉）：{after_a:?} vs {s:?}"
        );
        assert!(
            max_abs_diff(s, &anchor) > POS_TOL,
            "间隙位置不得退回启动 anchor {anchor:?}"
        );
    }

    // chunk B：从当前位继续（**无跳变**——首点即当前位）。
    let moves_b = [(0_usize, 0.45_f64), (1, 0.05)];
    let layout_left_b = layout_of(&ctx.left);
    let layout_right_b = layout_of(&ctx.right);
    let (cur_left, cur_right) = (positions(&ctx.left).await, positions(&ctx.right).await);
    let clock = robot_core::time::SystemClock::new();
    let sample_chan = ctx.left.channel.clone();
    let hw = ctx.left.hardware_id;
    let sample_task = tokio::spawn(async move {
        let mut seq = Vec::new();
        for _ in 0..14 {
            seq.push(get_state(&sample_chan, hw).await.position_rad);
            tokio::time::sleep(Duration::from_millis(80)).await;
        }
        seq
    });
    let run = run_chunk_pair(
        &mut pair,
        ActionSpec::scripted(chunk_payload(&layout_left_b, &cur_left, &moves_b)),
        ActionSpec::scripted(chunk_payload(&layout_right_b, &cur_right, &moves_b)),
        &clock,
        &pair_cfg(),
    )
    .await
    .expect("chunk B");
    assert!(run.result.is_pair_success());
    let traj_samples = sample_task.await.expect("sample task");

    // 无跳变：相邻采样步长 ≤ 单段位移的数倍（段位移 = Δ/CHUNK_STEPS）。
    #[allow(clippy::cast_precision_loss)]
    let steps = CHUNK_STEPS as f64;
    let step_limit = (0.45 - after_a[0]).abs() / steps * 4.0 + POS_TOL;
    for w in traj_samples.windows(2) {
        let d = (w[1][0] - w[0][0]).abs();
        assert!(
            d <= step_limit,
            "相邻采样跳变 {d} > 上限 {step_limit}（采样 = {traj_samples:?}）"
        );
    }
    ctx.shutdown().await.expect("shutdown clean");
}

// ============================================================================
// AC-E18：终态后 idle hold target 规则矩阵（RB5：各分支独立 ctx）
// ============================================================================

/// 先 Success 一次（hold target = 末点），再 **cancel** 一条 Scripted →
/// hold 必须 reset 回启动 anchor。
#[tokio::test]
async fn post_success_reset_matrix_cancel() {
    let ctx = DualCtx::bring_up_with("matrix_cancel", DualCtxOptions::scripted_only())
        .await
        .expect("bring up");
    let mut pair = arm_pair(&ctx);
    let anchor = positions(&ctx.left).await;

    run_success_pair(&ctx, &mut pair, &[(0_usize, 0.20_f64)]).await;
    tokio::time::sleep(SETTLE).await;
    let after_success = positions(&ctx.left).await;
    assert!(
        max_abs_diff(&after_success, &anchor) > POS_TOL,
        "先 Success"
    );

    // 再提交一条长 chunk 并 cancel（→ Cancelled / PartialSuccess，非 Success）。
    let layout = layout_of(&ctx.left);
    let current = positions(&ctx.left).await;
    let payload = chunk_payload(&layout, &current, &[(0_usize, 0.9_f64)]);
    let action_id = pair
        .left
        .invoke_action(ActionSpec::scripted(payload))
        .await
        .expect("提交");
    tokio::time::sleep(Duration::from_millis(120)).await;
    pair.left.cancel_task(&action_id).await.expect("cancel");
    tokio::time::sleep(SETTLE).await;

    let held = positions(&ctx.left).await;
    assert!(
        max_abs_diff(&held, &anchor) <= POS_TOL,
        "cancel 终态后 hold 必须 reset 回启动 anchor {anchor:?}，实得 {held:?}"
    );
    ctx.shutdown().await.expect("shutdown clean");
}

/// **failure 分支**（需 `test-fault-backend`）：fault 注入 → Failed →
/// Degraded hold == anchor → maintainer `ClearError` → 回 Idle。
#[tokio::test]
async fn post_success_reset_matrix_failure() {
    let ctx = DualCtx::bring_up_with("matrix_failure", DualCtxOptions::default())
        .await
        .expect("bring up（需 s1_daemon --features test-fault-backend）");
    let mut pair = arm_pair(&ctx);
    let anchor = positions(&ctx.left).await;

    run_success_pair(&ctx, &mut pair, &[(0_usize, 0.20_f64)]).await;
    tokio::time::sleep(SETTLE).await;

    // DwellMs + NO_PAYLOAD 在本 profile 下路由到 fault_inject（3 tick 后
    // InternalError）。
    let action_id = pair
        .left
        .invoke_action(ActionSpec::dwell_no_payload())
        .await
        .expect("fault action 提交");
    // 等终态。
    let mut terminal = None;
    for _ in 0..40 {
        let snap = pair.left.task_status(&action_id).await.expect("status");
        if let Some(t) = snap.phase.terminal() {
            terminal = Some((t, snap.failure_reason.clone()));
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let (state, reason) = terminal.expect("fault action 必须收敛到终态");
    assert_eq!(state, TerminalState::Failed);
    assert!(reason.contains("fault_inject"), "failure reason = {reason}");

    // Degraded 态 hold == 启动 anchor（D3 行为矩阵最后一行）。
    tokio::time::sleep(SETTLE).await;
    let held = positions(&ctx.left).await;
    assert!(
        max_abs_diff(&held, &anchor) <= POS_TOL,
        "Degraded hold 必须 == 启动 anchor {anchor:?}，实得 {held:?}"
    );

    // maintainer ClearError → 回 Idle（可再提交 Scripted 即为证据）。
    let mut client = RobotControlClient::new(ctx.left.channel.clone());
    client
        .clear_error(build_clear_error_command(
            Uuid::now_v7(),
            "matrix-failure-clear",
            "dimos_e2e",
            MAINTAINER_TOKEN,
            3_000,
            "AC-E18 failure 分支：Degraded → Idle",
        ))
        .await
        .expect("ClearError 必须成功");
    let layout = layout_of(&ctx.left);
    let current = positions(&ctx.left).await;
    pair.left
        .invoke_action(ActionSpec::scripted(chunk_payload(
            &layout,
            &current,
            &[(0_usize, 0.1_f64)],
        )))
        .await
        .expect("ClearError 后必须可再提交（已回 Idle）");
    ctx.shutdown().await.expect("shutdown clean");
}

/// **IK 分支**（5 维 solver 谱；宣称边界见 `dual_ctx` 模块 doc：只在**右臂**
/// 执行、只验证 reset 规则）。
#[tokio::test]
async fn post_success_reset_matrix_ik() {
    let ctx = DualCtx::bring_up_with("matrix_ik", DualCtxOptions::ik_branch())
        .await
        .expect("bring up");
    let mut pair = arm_pair(&ctx);
    let anchor = positions(&ctx.right).await;
    assert_eq!(anchor.len(), JointsSpec::IkSolver5.len());

    // 先 Success 一次（右臂）。
    let layout = layout_of(&ctx.right);
    let payload = chunk_payload(&layout, &anchor, &[(0_usize, 0.20_f64)]);
    let sid = pair
        .right
        .invoke_action(ActionSpec::scripted(payload))
        .await
        .expect("scripted 提交");
    wait_terminal(&mut pair.right, &sid).await;
    tokio::time::sleep(SETTLE).await;
    assert!(
        max_abs_diff(&positions(&ctx.right).await, &anchor) > POS_TOL,
        "先 Success 应离开 anchor"
    );

    // 再跑一条 IK（可达目标，沿 ik_backend_smoke 默认值）。
    let ik = ActionSpec {
        kind: ActionKindWire::Ik,
        payload_schema_id: robot_msg::schema_id::SCHEMA_IK_PAYLOAD,
        payload: build_ik_payload(0.20, 0.0, 0.10),
    };
    let ik_id = pair.right.invoke_action(ik).await.expect("IK 提交");
    wait_terminal(&mut pair.right, &ik_id).await;
    tokio::time::sleep(SETTLE).await;

    let held = positions(&ctx.right).await;
    assert!(
        max_abs_diff(&held, &anchor) <= POS_TOL,
        "IK 终态后 hold 必须 reset 回启动 anchor {anchor:?}，实得 {held:?}"
    );
    ctx.shutdown().await.expect("shutdown clean");
}

/// **HoldPose 分支**：Success 后跑 HoldPose 再 cancel → 仍 reset 回 anchor
/// （HoldPose 自身 0 改，每 tick 读 `anchor_bias`）。
#[tokio::test]
async fn post_success_reset_matrix_hold_pose() {
    let ctx = DualCtx::bring_up_with("matrix_hold", DualCtxOptions::scripted_only())
        .await
        .expect("bring up");
    let mut pair = arm_pair(&ctx);
    let anchor = positions(&ctx.left).await;

    run_success_pair(&ctx, &mut pair, &[(0_usize, 0.20_f64)]).await;
    tokio::time::sleep(SETTLE).await;

    let hold_spec = ActionSpec {
        kind: ActionKindWire::HoldPose,
        payload_schema_id: NO_PAYLOAD_SCHEMA_ID,
        payload: Vec::new(),
    };
    let hid = pair
        .left
        .invoke_action(hold_spec)
        .await
        .expect("HoldPose 提交");
    tokio::time::sleep(Duration::from_millis(200)).await;
    // HoldPose 期间 target 恒 = anchor_bias（B10 边界，0 改）。
    let during = positions(&ctx.left).await;
    assert!(
        max_abs_diff(&during, &anchor) <= POS_TOL,
        "HoldPose 期间必须 hold 在 anchor_bias"
    );
    pair.left.cancel_task(&hid).await.expect("cancel");
    wait_terminal(&mut pair.left, &hid).await;
    tokio::time::sleep(SETTLE).await;
    let held = positions(&ctx.left).await;
    assert!(
        max_abs_diff(&held, &anchor) <= POS_TOL,
        "HoldPose 终态后仍 reset 回 anchor"
    );
    ctx.shutdown().await.expect("shutdown clean");
}

// ============================================================================
// AC-E6：state 过期 → Scripted start 拒（不 fallback 到启动 anchor）
// ============================================================================

#[tokio::test]
async fn start_pose_stale_rejected() {
    let ctx = DualCtx::bring_up_with("stale", DualCtxOptions::scripted_only())
        .await
        .expect("bring up");
    let mut pair = arm_pair(&ctx);
    let layout = layout_of(&ctx.left);
    let current = positions(&ctx.left).await;
    let pid = ctx.left.s0_pid().expect("s0 pid");

    // SIGSTOP 真 s0 → state 停更 → 超过 max_stale_ms 即 stale。
    // SAFETY: pid 来自 Child::id()；SIGSTOP 是 async-signal-safe。
    unsafe { libc::kill(i32::try_from(pid).expect("pid"), libc::SIGSTOP) };
    tokio::time::sleep(STALE_WAIT).await;

    let payload = chunk_payload(&layout, &current, &[(0_usize, 0.2_f64)]);
    let err = pair
        .left
        .invoke_action(ActionSpec::scripted(payload.clone()))
        .await
        .expect_err("state 过期时 Scripted 必须被拒");
    // **明确拒绝**（FailedPrecondition 在白名单内）——不 fallback 到启动 anchor。
    assert!(err.is_definite(), "stale 拒绝应为明确拒绝，实得 {err:?}");
    match err {
        SubmitFailure::DefiniteRejected(e) => {
            assert_eq!(e.code(), Some("FailedPrecondition"), "{e}");
        }
        SubmitFailure::Indeterminate(e) => {
            panic!("stale 拒绝必须是明确拒绝（可证明未启动），实得 Indeterminate({e})")
        }
    }

    // SIGCONT 恢复 → 重新可提交。
    // SAFETY: 同上。
    unsafe { libc::kill(i32::try_from(pid).expect("pid"), libc::SIGCONT) };
    tokio::time::sleep(SETTLE).await;
    let current = positions(&ctx.left).await;
    let payload = chunk_payload(&layout, &current, &[(0_usize, 0.2_f64)]);
    let id = pair
        .left
        .invoke_action(ActionSpec::scripted(payload))
        .await
        .expect("恢复后必须可提交");
    wait_terminal(&mut pair.left, &id).await;
    ctx.shutdown().await.expect("shutdown clean");
}

// ============================================================================
// AC-E14：双臂协议部分失败全路径
// ============================================================================

#[tokio::test]
async fn pair_partial_failure_paths() {
    let ctx = DualCtx::bring_up_with("partial", DualCtxOptions::default())
        .await
        .expect("bring up（需 s1_daemon --features test-fault-backend）");
    let mut pair = arm_pair(&ctx);
    let clock = robot_core::time::SystemClock::new();
    let cfg = pair_cfg();

    // ---- (1) 提交期部分失败：一侧空 waypoints（backend Rejected →
    //          FailedPrecondition = 白名单内明确拒绝），另一侧正常 ----
    let (layout_left, layout_right) = (layout_of(&ctx.left), layout_of(&ctx.right));
    let cur_left = positions(&ctx.left).await;
    let err = run_chunk_pair(
        &mut pair,
        ActionSpec::scripted(chunk_payload(
            &layout_left,
            &cur_left,
            &[(0_usize, 0.2_f64)],
        )),
        ActionSpec::scripted(build_scripted_payload(&[])), // 空 waypoints → 拒
        &clock,
        &cfg,
    )
    .await
    .expect_err("一收一拒");
    match err {
        PairError::SubmitPartialFailure {
            rejected_side,
            ref cancelled,
            ..
        } => {
            assert_eq!(rejected_side, Side::Right);
            assert_eq!(cancelled.side, Side::Left, "已接收侧必须被 cancel 收敛");
        }
        other => panic!("expected SubmitPartialFailure, got {other:?}"),
    }

    // ---- (2) 执行期不对称：左 fault（3 tick 后 Failed）+ 右长 chunk ----
    tokio::time::sleep(SETTLE).await;
    let cur_right2 = positions(&ctx.right).await;
    let run = run_chunk_pair(
        &mut pair,
        ActionSpec::dwell_no_payload(), // → fault_inject
        ActionSpec::scripted(chunk_payload(
            &layout_right,
            &cur_right2,
            &[(0_usize, 0.6_f64)],
        )),
        &clock,
        &cfg,
    )
    .await
    .expect("协议必须收敛（非成功但不报错）");
    match &run.result {
        PairResult::BothUnsuccessful { left, right } => {
            assert_eq!(left.state, TerminalState::Failed);
            assert!(left.failure_reason.contains("fault_inject"));
            assert_eq!(
                right.state,
                TerminalState::Cancelled,
                "失败侧先到 → 幸存侧必须被 cancel"
            );
        }
        other => panic!("expected BothUnsuccessful, got {other:?}"),
    }
    assert!(!run.result.is_pair_success());
    ctx.shutdown().await.expect("shutdown clean");
}

// ============================================================================
// AC-E9：非目标关节填当前位 + 漂移容差
// ============================================================================

#[tokio::test]
async fn nontarget_fill_current() {
    let ctx = DualCtx::bring_up_with("fill_current", DualCtxOptions::scripted_only())
        .await
        .expect("bring up");
    let mut pair = arm_pair(&ctx);
    let gripper = ctx.options.joints.gripper_index().expect("gripper");

    // 先把臂关节挪离 0，让"填当前位"与"填 0"可区分。
    run_success_pair(&ctx, &mut pair, &[(1_usize, 0.30_f64), (3, -0.25)]).await;
    tokio::time::sleep(SETTLE).await;
    let before = positions(&ctx.left).await;

    // 只动 gripper：其余 5 关节必须保持 `before`（D8 填当前位）。
    run_success_pair(&ctx, &mut pair, &[(gripper, 0.40_f64)]).await;
    tokio::time::sleep(SETTLE).await;
    let after = positions(&ctx.left).await;

    for (i, (b, a)) in before.iter().zip(&after).enumerate() {
        if i == gripper {
            assert!((a - 0.40).abs() <= POS_TOL, "gripper 必须到位：{a}");
        } else {
            assert!(
                (a - b).abs() <= POS_TOL,
                "非目标关节 {i} 必须保持当前位：{b} → {a}（容差 {POS_TOL}）"
            );
        }
    }
    ctx.shutdown().await.expect("shutdown clean");
}

// ============================================================================
// AC-E2 跨进程：S0 丢弃错 layout 命令（**独立 mini 拓扑** + raw 注入器）
// ============================================================================

/// R3-B4 锁死的场景：**不起对应 S1**（正常 S1 持续发合法命令 + `latest()`
/// 塌缩会造成假 PASS），拓扑 = s0 + FakeHeartbeat + raw 注入器。
///
/// 步骤（含 RB5 要求的**正向对照**）：
/// ① 等 s0 就绪 → ② 注入 **matching-layout** 命令并断言输出**确实变化**
/// （证明注入器与判据通路有效）→ ③ 注入 ≥3 帧**错 layout**、target = 前值 +
/// 明显偏移 → settle → 断言**保持 ② 后的值不变**。
#[tokio::test]
async fn s0_drops_wrong_layout_commands() {
    let _guard = global_lock().await;
    let pid = std::process::id();
    let robot_id = format!("dimos_e2e_wrong_layout_{pid}");
    let hardware_id = "left_arm";
    let dir = std::path::PathBuf::from(format!("/tmp/dimos_e2e/{robot_id}"));
    tokio::fs::create_dir_all(&dir).await.expect("mkdir");

    let joints = ["j0", "j1", "j2"];
    let config = dir.join("config.toml");
    let cfg_text = format!(
        "[robot]\nid = \"{robot_id}\"\nconfig_version = \"0.5.0\"\n\n\
         [[hardware]]\nid = \"{hardware_id}\"\nadapter = \"mock_hard_rt\"\n\
         joints = [\"{}\", \"{}\", \"{}\"]\nmotor_ids = [1, 2, 3]\nauto_enable = true\n",
        joints[0], joints[1], joints[2]
    );
    tokio::fs::write(&config, cfg_text)
        .await
        .expect("write config");
    let latch = dir.join("estop_latch");
    tokio::fs::write(&latch, b"0\n").await.expect("latch");
    tokio::fs::write(latch.with_extension("bak"), b"0\n")
        .await
        .expect("latch bak");

    let layout = load_hardware_layout(&config, hardware_id).expect("layout");
    let s0_bin = common::subprocess::locate_test_bin("s0_daemon").expect("s0 bin");
    let mut s0 = common::subprocess::SubprocessHandle::spawn(
        "s0_daemon",
        common::subprocess::build_s0_daemon_command(
            &s0_bin,
            &robot_id,
            hardware_id,
            &config,
            &latch,
        ),
    )
    .expect("spawn s0");
    s0.wait_for_marker("set_enabled_rr_server started", Duration::from_secs(8))
        .await
        .expect("s0 ready");
    // FakeHeartbeat 扮演 s1：把 s0 推到 Enabled.Active（命令才会被执行）。
    let hb = common::fake_heartbeat::FakeHeartbeatPublisher::spawn(&robot_id).expect("fake hb");
    tokio::time::sleep(SETTLE).await;

    let state_topic = format!("/{robot_id}/s0/{hardware_id}/state");
    let cmd_topic = format!("/{robot_id}/s1/{hardware_id}/command");
    let n = joints.len();

    // ② 正向对照：matching layout → 输出必须变化。
    let baseline = raw_s0::read_positions(state_topic.clone(), n, Duration::from_secs(3))
        .await
        .expect("baseline");
    let good_target = vec![0.30_f64, -0.20, 0.10];
    raw_s0::inject_joint_commands(cmd_topic.clone(), layout.layout_id, good_target.clone(), 5)
        .await
        .expect("inject matching layout");
    tokio::time::sleep(SETTLE).await;
    let after_good = raw_s0::read_positions(state_topic.clone(), n, Duration::from_secs(3))
        .await
        .expect("read after good");
    assert!(
        max_abs_diff(&after_good, &good_target) <= POS_TOL,
        "正向对照：matching layout 命令必须生效（期望 {good_target:?} 实得 {after_good:?}）"
    );
    assert!(
        max_abs_diff(&after_good, &baseline) > POS_TOL,
        "正向对照：输出必须确实变化"
    );

    // ③ 错 layout：≥3 帧、target 明显偏移 → 必须被丢弃。
    let bad_target: Vec<f64> = good_target.iter().map(|v| v + 0.5).collect();
    raw_s0::inject_joint_commands(
        cmd_topic,
        layout.layout_id ^ 0xDEAD_BEEF,
        bad_target.clone(),
        5,
    )
    .await
    .expect("inject wrong layout");
    tokio::time::sleep(SETTLE).await;
    let after_bad = raw_s0::read_positions(state_topic, n, Duration::from_secs(3))
        .await
        .expect("read after bad");
    assert!(
        max_abs_diff(&after_bad, &after_good) <= POS_TOL,
        "错 layout 命令必须被丢弃（应保持 {after_good:?}，实得 {after_bad:?}；\
         若变成 {bad_target:?} 说明 S0 command gate 失效）"
    );

    hb.shutdown();
    let status = s0
        .shutdown_with_dump(Duration::from_secs(3), dir.join("s0.log"))
        .await
        .expect("s0 shutdown");
    assert!(status.success(), "s0 exit = {status:?}");
}

// ============================================================================
// 私有 helper
// ============================================================================

/// 等单侧 action 到终态（有界；超时即 panic 带诊断）。
async fn wait_terminal(arm: &mut TonicArm, action_id: &str) {
    for _ in 0..80 {
        let snap = arm.task_status(action_id).await.expect("status");
        if snap.phase.terminal().is_some() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("action {action_id} 未在 4 s 内收敛到终态");
}

/// `IkPayload` fbs（inline 构造，沿 `ik_backend_smoke` 先例——reviewer 拍板
/// "smoke 不抽公共 helper"）。
fn build_ik_payload(x: f64, y: f64, z: f64) -> Vec<u8> {
    use flatbuffers::FlatBufferBuilder;
    use robot_msg::msg::{IkPayload, IkPayloadArgs};
    let mut fbb = FlatBufferBuilder::with_capacity(256);
    let xyz = fbb.create_vector::<f64>(&[x, y, z]);
    let quat = fbb.create_vector::<f64>(&[0.0, 0.0, 0.0, 1.0]);
    let seed = fbb.create_vector::<f64>(&[]);
    let frame = fbb.create_string("");
    let payload = IkPayload::create(
        &mut fbb,
        &IkPayloadArgs {
            target_xyz: Some(xyz),
            target_quat_xyzw: Some(quat),
            seed_joints_rad: Some(seed),
            frame: Some(frame),
        },
    );
    fbb.finish(payload, None);
    fbb.finished_data().to_vec()
}
