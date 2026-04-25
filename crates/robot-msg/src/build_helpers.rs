//! `FlatBuffers` body builder —— 非 RT 路径构造 `JointState` / `JointCommand` /
//! `Heartbeat` / `EStopEvent` body bytes 的统一入口。
//!
//! ### RT 约束
//!
//! 本模块的所有函数**严禁在 RT 线程调用**（`docs/00_design.md` §0 / §4.9）。
//! `FlatBufferBuilder::new` 会分配 `Vec`，`create_vector` / `create_string` 会
//! 可能 realloc；`finish` + `finished_data().to_vec()` 再 `memcpy` 一次。
//!
//! 合法调用方：
//! - `robot-s1::mock_s1`（pub `JointCommand` / `Heartbeat`）
//! - `robot-s0::pub_thread`（pub `JointState` / `Heartbeat`，非 RT）
//! - `robot-s0::ipc_thread`（收 `EStop` RPC 后发 `EStopEvent` topic，非 RT）
//! - `robot-tests::hello_pubsub`、`iceoryx2_rt_baseline` 等测试 binary
//!
//! ### 返回值
//!
//! 所有函数返回 `Vec<u8>`。调用方可以 `SampleWriter::set_body_bytes(&vec)`
//! 一次 memcpy 到 iceoryx2 loan 内存。如果未来想节省一次拷贝，可以加
//! `build_into_writer(&mut w, args)` 的变体——但 Step 6 Part 2 scope 内不做。
//!
//! ### 测试
//!
//! 每个 builder 有一条 round-trip 测试：build → `flatbuffers::root::<T>(bytes)`
//! 解析回来，确认字段值。

use flatbuffers::FlatBufferBuilder;

use crate::msg::{
    ControlMode, EStopEvent, EStopEventArgs, Heartbeat, HeartbeatArgs, JointCommand,
    JointCommandArgs, JointState, JointStateArgs, S0ControlLink, S0FaultCode, S0Mode, S0OutputMode,
    S0SafetyState, StaleAction,
};

// ============================================================================
// JointState（`robot.msg.JointState`）
// ============================================================================

/// 构造 `JointState` body bytes（仅关节字段；S0 自身状态字段 = Unknown）。
///
/// - `layout_id` 来自上游 `JointLayout`。demo / test 常用 `0xCAFE_BABE_DEAD_BEEF`。
/// - `positions` 长度应与 `JointLayout.names` 相同；本函数不校验。
/// - `velocities` / `torques` 为空 slice 时写入 `None`（不占字节）。
/// - `frame_id` 为空字符串时写入 `None`。
/// - `s0_mode` / `s0_control_link` / `s0_output_mode` / `s0_safety_state` /
///   `s0_fault_code` 全部留 `Unknown`（`FlatBuffers` 默认值不写 vtable）。
///   想带 S0 状态用 [`build_joint_state_with_s0`].
#[must_use]
pub fn build_joint_state(
    layout_id: u64,
    stamp_ns: u64,
    positions: &[f64],
    velocities: &[f64],
    torques: &[f64],
    frame_id: &str,
) -> Vec<u8> {
    build_joint_state_with_s0(
        layout_id,
        stamp_ns,
        positions,
        velocities,
        torques,
        frame_id,
        S0Mode::Unknown,
        S0ControlLink::Unknown,
        S0OutputMode::Unknown,
        S0SafetyState::TorqueUnknown,
        S0FaultCode::Unknown,
    )
}

/// 构造 `JointState` body bytes，**带 S0 自身状态字段**（task-30 v9 pre-2）。
///
/// `pub_thread.publish_joint_state` 调本变体把当前 `S0StateSnapshot` 镜像
/// 写进 wire body，让外部订阅者一次拿到 joint + S0 mode + `safety_state`。
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn build_joint_state_with_s0(
    layout_id: u64,
    stamp_ns: u64,
    positions: &[f64],
    velocities: &[f64],
    torques: &[f64],
    frame_id: &str,
    s0_mode: S0Mode,
    s0_control_link: S0ControlLink,
    s0_output_mode: S0OutputMode,
    s0_safety_state: S0SafetyState,
    s0_fault_code: S0FaultCode,
) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::with_capacity(256);
    let pos = fbb.create_vector::<f64>(positions);
    let vel = if velocities.is_empty() {
        None
    } else {
        Some(fbb.create_vector::<f64>(velocities))
    };
    let eff = if torques.is_empty() {
        None
    } else {
        Some(fbb.create_vector::<f64>(torques))
    };
    let frame = if frame_id.is_empty() {
        None
    } else {
        Some(fbb.create_string(frame_id))
    };
    let js = JointState::create(
        &mut fbb,
        &JointStateArgs {
            layout_id,
            position_rad: Some(pos),
            velocity_rps: vel,
            effort_nm: eff,
            stamp_ns,
            frame_id: frame,
            s0_mode,
            s0_control_link,
            s0_output_mode,
            s0_safety_state,
            s0_fault_code,
        },
    );
    fbb.finish(js, None);
    fbb.finished_data().to_vec()
}

// ============================================================================
// JointCommand（`robot.msg.JointCommand`）
// ============================================================================

/// 构造 `JointCommand` body bytes。
///
/// - `mode` 用 flatc 生成的 enum（`crate::msg::ControlMode`）。
/// - `target` 长度与 `JointLayout.names` 相同。
/// - `deadline_ns` 基于 `CLOCK_MONOTONIC_RAW`；超过此时刻的命令在 S0 侧丢弃。
/// - `priority`：0..=255，大者优先（§3.5）。默认场景用 10。
/// - `on_timeout`：heartbeat 失联后此条命令的降级策略。默认 `Hold`。
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn build_joint_command(
    layout_id: u64,
    mode: ControlMode,
    target: &[f64],
    deadline_ns: u64,
    priority: u8,
    on_timeout: StaleAction,
    source_task_id: u64,
) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::with_capacity(256);
    let tgt = fbb.create_vector::<f64>(target);
    let cmd = JointCommand::create(
        &mut fbb,
        &JointCommandArgs {
            layout_id,
            mode,
            target: Some(tgt),
            deadline_ns,
            priority,
            source_task_id,
            on_timeout,
        },
    );
    fbb.finish(cmd, None);
    fbb.finished_data().to_vec()
}

// ============================================================================
// Heartbeat（`robot.msg.Heartbeat`）
// ============================================================================

/// 构造 `Heartbeat` body bytes。
///
/// - `source_id`：约定 `1 = S0`, `2 = S1`, `100.. = policy_host`.
/// - `seq`：单调递增；订阅端用 seq 判是否有漏。
/// - `tick_rate_hz`：方便订阅端快速校对周期；S1 = 50.0。
/// - `stamp_ns`：`CLOCK_MONOTONIC_RAW`。
#[must_use]
pub fn build_heartbeat(source_id: u32, seq: u64, tick_rate_hz: f64, stamp_ns: u64) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::with_capacity(64);
    let hb = Heartbeat::create(
        &mut fbb,
        &HeartbeatArgs {
            source_id,
            seq,
            tick_rate_hz,
            stamp_ns,
        },
    );
    fbb.finish(hb, None);
    fbb.finished_data().to_vec()
}

// ============================================================================
// EStopEvent（`robot.msg.EStopEvent`）
// ============================================================================

/// 构造 `EStopEvent` body bytes。
///
/// - `reason`：人读字符串，保留到 recorder / RPC 回执。
/// - `source`：`"button"` / `"rpc"` / `"s1_detect"` 三种之一（§3.5 约定）。
/// - `latched`：S0 是否已把 latch 持久化到磁盘（§6.6）。
#[must_use]
pub fn build_estop_event(reason: &str, stamp_ns: u64, source: &str, latched: bool) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::with_capacity(128);
    let reason_off = fbb.create_string(reason);
    let source_off = fbb.create_string(source);
    let ev = EStopEvent::create(
        &mut fbb,
        &EStopEventArgs {
            reason: Some(reason_off),
            stamp_ns,
            source: Some(source_off),
            latched,
        },
    );
    fbb.finish(ev, None);
    fbb.finished_data().to_vec()
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use flatbuffers::root;

    #[test]
    fn joint_state_round_trip() {
        let bytes = build_joint_state(
            0xDEAD_BEEF_0000_0001,
            1_234_567_890,
            &[0.1, 0.2, 0.3],
            &[0.01, 0.02, 0.03],
            &[0.001, 0.002, 0.003],
            "world",
        );
        let js = root::<JointState<'_>>(&bytes).unwrap();
        assert_eq!(js.layout_id(), 0xDEAD_BEEF_0000_0001);
        assert_eq!(js.stamp_ns(), 1_234_567_890);
        let pos: Vec<f64> = js.position_rad().unwrap().iter().collect();
        assert_eq!(pos, vec![0.1, 0.2, 0.3]);
        let vel: Vec<f64> = js.velocity_rps().unwrap().iter().collect();
        assert_eq!(vel, vec![0.01, 0.02, 0.03]);
        let eff: Vec<f64> = js.effort_nm().unwrap().iter().collect();
        assert_eq!(eff, vec![0.001, 0.002, 0.003]);
        assert_eq!(js.frame_id(), Some("world"));
    }

    #[test]
    fn joint_state_omits_empty_vectors_and_frame() {
        let bytes = build_joint_state(0xCAFE, 0, &[1.0], &[], &[], "");
        let js = root::<JointState<'_>>(&bytes).unwrap();
        assert!(js.velocity_rps().is_none());
        assert!(js.effort_nm().is_none());
        assert!(js.frame_id().is_none());
    }

    #[test]
    fn joint_command_round_trip() {
        let bytes = build_joint_command(
            0xCAFE_0001,
            ControlMode::Position,
            &[0.5, -0.5],
            999_999_999,
            42,
            StaleAction::Hold,
            7,
        );
        let cmd = root::<JointCommand<'_>>(&bytes).unwrap();
        assert_eq!(cmd.layout_id(), 0xCAFE_0001);
        assert_eq!(cmd.mode(), ControlMode::Position);
        let tgt: Vec<f64> = cmd.target().unwrap().iter().collect();
        assert_eq!(tgt, vec![0.5, -0.5]);
        assert_eq!(cmd.deadline_ns(), 999_999_999);
        assert_eq!(cmd.priority(), 42);
        assert_eq!(cmd.source_task_id(), 7);
        assert_eq!(cmd.on_timeout(), StaleAction::Hold);
    }

    #[test]
    fn heartbeat_round_trip() {
        let bytes = build_heartbeat(2, 1_234, 50.0, 111_222_333);
        let hb = root::<Heartbeat<'_>>(&bytes).unwrap();
        assert_eq!(hb.source_id(), 2);
        assert_eq!(hb.seq(), 1_234);
        assert!((hb.tick_rate_hz() - 50.0).abs() < 1e-9);
        assert_eq!(hb.stamp_ns(), 111_222_333);
    }

    #[test]
    fn estop_event_round_trip() {
        let bytes = build_estop_event("physical button", 555, "button", true);
        let ev = root::<EStopEvent<'_>>(&bytes).unwrap();
        assert_eq!(ev.reason(), Some("physical button"));
        assert_eq!(ev.stamp_ns(), 555);
        assert_eq!(ev.source(), Some("button"));
        assert!(ev.latched());
    }
}
