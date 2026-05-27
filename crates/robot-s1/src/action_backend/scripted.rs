//! v0.4 候选 1 `ScriptedBackend`：joint-space 线性插值 + waypoint dwell。
//!
//! ## 行为
//!
//! - `name()` = `"scripted"`（profile `[s1.profile.backends]` 引用名）。
//! - `supported_kinds()` = `&[ActionKind::Scripted]`（v0.4 (1/3) atomic
//!   land 后的新 variant；详 `crates/robot-s1/src/action.rs`）。
//! - `supported_schemas()` = `&[SCHEMA_SCRIPTED_PAYLOAD]`（`0x0000_040B`；
//!   **不**含 `NO_PAYLOAD_SCHEMA_ID`，跟 hold_pose 显著区分）。
//! - `start(req, ctx)`：四项 semantic check（详 ## semantic check 段）→
//!   一次性解码 `ScriptedPayload` 到 `Vec<DecodedWaypoint>` + snapshot
//!   `ctx.anchor_bias.to_vec()` 作 `initial_pose` → 构造 [`ScriptedInstance`]
//!   返 `Box<dyn ActionInstance>`。
//! - `tick(ctx)`：phase 状态机推进（Interp → Dwell → next waypoint）+
//!   线性插值算 target positions → 复用
//!   [`crate::coordinator::build_hold_command`] 构 body bytes（
//!   source_task_id == `ctx.command_seq`，B3 §3.7 单调契约）→ 末段 Dwell
//!   完返 `Completed(ActionOutcome::Success)`，中间段返
//!   `Continue { progress, command: Some(body) }`。
//! - `cancel()`：cooperative，set flag；下次 tick 返
//!   `Completed(ActionOutcome::PartialSuccess { warning: "cancelled_at_waypoint_N" })`
//!   （Q4(a) 拍板：backend lib 返 PartialSuccess，coordinator
//!   `pending_cancel` 路径转 `ActionEvent::Cancelled` 写 stream）。
//!
//! ## semantic check（`start()` 期；reviewer Plan round 3 拍板 only byte cap）
//!
//! `MAX_SCRIPTED_PAYLOAD_BYTES = 4 KiB` 是**唯一**容量上限（gateway
//! byte cap，详 `crates/robot-rpc/src/task_handlers.rs`）。
//! `ScriptedBackend::start()` **不**做 `decoded_waypoints.len()` count
//! check / `> 100` 拒绝路径；只做四项：
//!
//! 1. `payload_schema_id == SCHEMA_SCRIPTED_PAYLOAD`（dispatch 守门已保，
//!    本步 defensive 再断言；不匹配 → `ActionStartError::Rejected`）
//! 2. `flatbuffers::root::<ScriptedPayload>(payload)` decode 成功（含
//!    fbs verify）；失败 → `ActionStartError::PayloadDecode`
//! 3. `waypoints` 非空（empty → `ActionStartError::Rejected`）
//! 4. 每段 `positions.len() == hardware.joints.len()` +
//!    `segment_duration_ms > 0`；任一段不满足 → `ActionStartError::Rejected`
//!
//! ## source_task_id 单调契约（B3 §3.7）
//!
//! Scripted body 的 `source_task_id` **必须**等于 `ctx.command_seq`，
//! 由 `run_action_tick_loop` 透传当前 publish frame 的 `TickState.cmd_seq`
//! 进 `ActionContext.command_seq`。**不**允许 backend 私有 counter 从
//! 0 起增 —— 那会让 S0 `ipc_thread::join_command_to_buf` 把
//! `fb.source_task_id()` 写到 `JointCommandBuf.seq` 时看到 seq 回退，
//! 破坏 `adapter.rs` 单调递增契约。沿 hold_pose B3 round 2 阻断 1 修法。
//!
//! ## segment from/to 来源（reviewer Plan round 2 阻断 2 拍板）
//!
//! - segment 0 的 `from` = `start()` 期 snapshot 的
//!   `initial_pose = ctx.anchor_bias.to_vec()`
//! - segment i>0 的 `from` = `decoded_waypoints[i-1].positions`
//! - 任一 segment 的 `to` = `decoded_waypoints[i].positions`
//!
//! `anchor_bias` 仅用作启动期 initial pose 来源；tick 期不再使用
//! `ctx.anchor_bias`，每次 tick 复用 `self.initial_pose` /
//! `self.decoded_waypoints` 内部 owned 数据。
//!
//! ## progress 语义化（Q3(c) 组合拍板）
//!
//! `progress = (self.current_idx as f32 + segment_progress) /
//! total_waypoints as f32`；其中 `segment_progress ∈ [0.0, 1.0]` 是当前
//! segment 内的时间百分比（Interp 期）或 1.0（Dwell 期）。
//! `current_idx` 是已**完成**的 waypoint 数；末段 Dwell 完之后
//! progress 自然到 1.0 + Completed。
//!
//! ## tick_buf 复用（Q7 拍板）
//!
//! `self.tick_buf: Vec<f64>` 启动期 `Vec::with_capacity(hardware.joints.len())`
//! 预分配，tick 期 `clear()` + extend；**避免** per-tick alloc。
//! Hardware joint 数在 backend 生命周期内不变（`HardwareSection` 启动
//! 期固化），不需 grow 防护。
//!
//! ## inventory 注册
//!
//! 整文件 `#[cfg(feature = "backend-scripted")]` gate（Q17(a) 拍板默
//! 认 on）。Feature off 时整文件不 link → `BackendRegistry::from_inventory`
//! 收集不到 scripted → `[s1.profile.backends = ["scripted"]]` 在
//! `validate_profile_section` 报 `UnknownBackend`，符合"配置与编译
//! profile 不匹配启动期 fail-fast"硬约束。

#![allow(clippy::doc_markdown)]

use crate::action::{
    ActionBackend, ActionCancelError, ActionContext, ActionFailure, ActionInstance, ActionKind,
    ActionOutcome, ActionRequest, ActionStartError, ActionTickOutput, JointCommandBody,
};
use crate::coordinator::build_hold_command;
use robot_msg::msg::ScriptedPayload as FbScriptedPayload;
use robot_msg::schema_id::SCHEMA_SCRIPTED_PAYLOAD;

// ============================================================================
// ScriptedBackend
// ============================================================================

/// v0.4 候选 1 第一个带 payload 的真 action backend：joint-space 线性
/// 插值 + waypoint dwell。
///
/// **零字段** struct：所有状态在 [`ScriptedInstance`] 上；backend 本身
/// 仅声明能力 + 解码 payload + 创建 instance。
#[derive(Debug, Clone, Copy)]
pub struct ScriptedBackend;

#[allow(clippy::unnecessary_literal_bound)]
impl ActionBackend for ScriptedBackend {
    fn name(&self) -> &str {
        "scripted"
    }

    fn supported_kinds(&self) -> &[ActionKind] {
        const KINDS: &[ActionKind] = &[ActionKind::Scripted];
        KINDS
    }

    fn supported_schemas(&self) -> &[u32] {
        // **不**含 NO_PAYLOAD_SCHEMA_ID：Scripted 强制带 payload；
        // 跟 hold_pose / mock_dwell 显著区分。
        const SCHEMAS: &[u32] = &[SCHEMA_SCRIPTED_PAYLOAD];
        SCHEMAS
    }

    fn start(
        &self,
        req: &ActionRequest,
        ctx: &ActionContext<'_>,
    ) -> Result<Box<dyn ActionInstance>, ActionStartError> {
        // (1) defensive payload_schema_id check（dispatch 已守门，这里
        // 再防御性确认；如未来 dispatch 路径漂移可立刻定位）。
        if req.payload_schema_id != SCHEMA_SCRIPTED_PAYLOAD {
            return Err(ActionStartError::Rejected(format!(
                "ScriptedBackend expects payload_schema_id == SCHEMA_SCRIPTED_PAYLOAD \
                 ({SCHEMA_SCRIPTED_PAYLOAD:#010x}), got {:#010x}",
                req.payload_schema_id
            )));
        }

        // (2) fbs decode + verify（root::<FbScriptedPayload> 已含
        // verify）。任何 verify fail / 字段读失败走 PayloadDecode 路径。
        let fb = flatbuffers::root::<FbScriptedPayload<'_>>(&req.payload).map_err(|e| {
            ActionStartError::PayloadDecode(format!(
                "ScriptedPayload fbs verify failed ({} bytes): {e}",
                req.payload.len()
            ))
        })?;

        // (3) waypoints 非空。
        let fb_wps = fb.waypoints().ok_or_else(|| {
            ActionStartError::Rejected(
                "ScriptedPayload.waypoints must be present (got null vector)".into(),
            )
        })?;
        if fb_wps.is_empty() {
            return Err(ActionStartError::Rejected(
                "ScriptedPayload.waypoints must be non-empty".into(),
            ));
        }

        // (4) 每段 positions.len() == hardware.joints.len() +
        // segment_duration_ms > 0。decode 期一次性 owned 拷贝到
        // Vec<DecodedWaypoint>；tick 期不再解 fbs（Q11 拍板）。
        let expected_joints = ctx.hardware.joints.len();
        let mut decoded_waypoints: Vec<DecodedWaypoint> = Vec::with_capacity(fb_wps.len());
        for (idx, wp) in fb_wps.iter().enumerate() {
            let positions: Vec<f64> = wp.positions().map_or_else(Vec::new, |v| v.iter().collect());
            if positions.len() != expected_joints {
                return Err(ActionStartError::Rejected(format!(
                    "ScriptedPayload.waypoints[{idx}].positions length {} != \
                     hardware.joints.len() {expected_joints}",
                    positions.len()
                )));
            }
            let segment_duration_ms = wp.segment_duration_ms();
            if segment_duration_ms == 0 {
                return Err(ActionStartError::Rejected(format!(
                    "ScriptedPayload.waypoints[{idx}].segment_duration_ms must be > 0"
                )));
            }
            decoded_waypoints.push(DecodedWaypoint {
                positions,
                dwell_ms: wp.dwell_ms(),
                segment_duration_ms,
            });
        }

        // segment 0 from = snapshot 自 ctx.anchor_bias.to_vec()
        // （reviewer Plan round 2 阻断 2 修法：anchor_bias 是启动期
        // initial pose 来源，不作 tick 持续动态 target）。
        // 长度必须跟 hardware.joints.len() 一致（coordinator 启动期
        // anchor 同款约束；此处 defensive 断言）。
        let initial_pose: Vec<f64> = ctx.anchor_bias.to_vec();
        if initial_pose.len() != expected_joints {
            return Err(ActionStartError::Rejected(format!(
                "ScriptedBackend: ctx.anchor_bias length {} != hardware.joints.len() {expected_joints}",
                initial_pose.len()
            )));
        }

        let instance = ScriptedInstance {
            cancelled: false,
            decoded_waypoints,
            initial_pose,
            current_idx: 0,
            phase: Phase::Interp { start_ns: 0 },
            phase_started: false,
            tick_buf: Vec::with_capacity(expected_joints),
        };
        Ok(Box::new(instance))
    }
}

// ============================================================================
// DecodedWaypoint
// ============================================================================

/// `ScriptedBackend::start()` 期一次性从 fbs 解码出来的 owned waypoint
/// 数据。tick 期不再访问 `ActionRequest.payload` / fbs root，避免
/// per-tick decode 开销（Q11 拍板）。
#[derive(Debug, Clone)]
struct DecodedWaypoint {
    /// 目标关节位置（rad）；长度 == `hardware.joints.len()`（`start()` 守门）。
    positions: Vec<f64>,
    /// 到位后停留 ms（0 = 不 dwell 直接进下一段）。
    dwell_ms: u32,
    /// 当前段线性插值总时长 ms（> 0；`start()` 守门）。
    segment_duration_ms: u32,
}

// ============================================================================
// Phase
// ============================================================================

/// `ScriptedInstance` 当前 segment 内的 phase。
///
/// 每个 waypoint 经历 `Interp` → `Dwell` → next waypoint（如果不是末
/// 段）；末段 `Dwell` 完后返 `Completed(Success)`。
#[derive(Debug, Clone, Copy)]
enum Phase {
    /// 线性插值阶段：从 `from` 到 `to` 走 `segment_duration_ms`。
    Interp {
        /// 进入本 phase 的 monotonic ns（首次 tick 时填）。
        start_ns: u64,
    },
    /// dwell 阶段：在 waypoint target 位置保持 `dwell_ms`。
    Dwell {
        /// 进入本 phase 的 monotonic ns（首次 tick 时填）。
        start_ns: u64,
    },
}

// ============================================================================
// ScriptedInstance
// ============================================================================

/// `ScriptedBackend::start` 返的 per-action 实例。
///
/// **owned 状态**：`decoded_waypoints` / `initial_pose` / `tick_buf` 在
/// `start()` 期 owned，tick 期复用；coordinator drop instance 时一起回收。
struct ScriptedInstance {
    /// `cancel()` 设置；下次 [`ActionInstance::tick`] 自查 + 收敛返
    /// `Completed(PartialSuccess { warning: "cancelled_at_waypoint_N" })`
    /// （Q4(a) 拍板）。
    cancelled: bool,
    /// `start()` 期一次性 owned decode；tick 期不再解 fbs。
    decoded_waypoints: Vec<DecodedWaypoint>,
    /// segment 0 的 `from`（snapshot 自 `ctx.anchor_bias.to_vec()`；
    /// reviewer Plan round 2 阻断 2 修法）。
    initial_pose: Vec<f64>,
    /// 当前正在跑的 waypoint index（segment 完成后 += 1；末段 dwell
    /// 完后等于 `decoded_waypoints.len()` 即 Completed 触发条件）。
    current_idx: usize,
    /// 当前 segment 内的 phase（`start_ns` 用 sentinel 0 + `phase_started`
    /// flag；首次 tick 用 `ctx.clock.now()` 填）。
    phase: Phase,
    /// `phase` 是否已用真 `start_ns` 初始化（首次 tick 把 sentinel 0
    /// 替换成 `ctx.clock.now()`；Phase enum 用 sentinel 而非 `Option`
    /// 避免每 tick match 多一层 unwrap）。
    phase_started: bool,
    /// tick 期目标位置缓冲（`with_capacity(hardware.joints.len())` 预
    /// 分配，tick 期 `clear()` + extend 复用；Q7 拍板避 per-tick alloc）。
    tick_buf: Vec<f64>,
}

impl ScriptedInstance {
    /// 计算 Q3(c) 组合 progress = `(current_idx + segment_progress) /
    /// total_waypoints`。`segment_progress ∈ [0.0, 1.0]`。
    #[allow(clippy::cast_precision_loss)]
    fn progress(&self, segment_progress: f32) -> f32 {
        let total = self.decoded_waypoints.len() as f32;
        let done = self.current_idx as f32;
        ((done + segment_progress) / total).clamp(0.0, 1.0)
    }

    /// cancel 收敛 outcome：`PartialSuccess { warning:
    /// "cancelled_at_waypoint_N" }`（Q4(a) 拍板）。
    fn cancel_outcome(&self) -> ActionOutcome {
        ActionOutcome::PartialSuccess {
            warning: format!("cancelled_at_waypoint_{}", self.current_idx),
        }
    }
}

impl ActionInstance for ScriptedInstance {
    fn tick(&mut self, ctx: &mut ActionContext<'_>) -> Result<ActionTickOutput, ActionFailure> {
        // (1) cancel 短路（Q4(a)）：backend lib 返 PartialSuccess；
        // coordinator pending_cancel 路径会转成 ActionEvent::Cancelled
        // 写事件流。
        if self.cancelled {
            return Ok(ActionTickOutput::Completed(self.cancel_outcome()));
        }

        let now_ns = ctx.clock.now().0;

        // (2) phase start_ns 延迟初始化（首次 tick 拿真 clock 填）。
        if !self.phase_started {
            self.phase = match self.phase {
                Phase::Interp { .. } => Phase::Interp { start_ns: now_ns },
                Phase::Dwell { .. } => Phase::Dwell { start_ns: now_ns },
            };
            self.phase_started = true;
        }

        // (3) phase 状态机推进 + 算 target positions + segment_progress。
        let current_wp = &self.decoded_waypoints[self.current_idx];
        let segment_duration_ns = u64::from(current_wp.segment_duration_ms) * 1_000_000;
        let dwell_ns = u64::from(current_wp.dwell_ms) * 1_000_000;

        let segment_progress: f32 = match self.phase {
            Phase::Interp { start_ns } => {
                let elapsed = now_ns.saturating_sub(start_ns);
                if elapsed >= segment_duration_ns {
                    // Interp 完成 → 切 Dwell（如果 dwell_ms > 0）或
                    // 直接推进到下一 waypoint（dwell_ms == 0）。
                    // 写 target = to（waypoint 终点位置），方便 cmd
                    // 输出落定后立刻 hold。
                    self.tick_buf.clear();
                    self.tick_buf.extend_from_slice(&current_wp.positions);
                    if current_wp.dwell_ms == 0 {
                        // 跳过 Dwell：直接 advance 到下一段（或
                        // Completed）。复用 advance_waypoint 逻辑。
                        return Ok(self.advance_waypoint_and_emit(ctx, now_ns));
                    }
                    // **关键**：Dwell start_ns 用 Interp 自然完成时刻
                    // （= original_interp_start + segment_duration_ns）
                    // **不**用 now_ns。这样大步推 clock（如 tick 间隔 >
                    // Interp 时长）也能让 Dwell elapsed 自然累计；下一
                    // tick 立即看到 Dwell 超时就 advance，不需要"夹一
                    // 帧重置 Dwell start"。tick_completed_after_last_dwell
                    // 测一帧推过总时长的场景靠此修法收敛到 Completed。
                    let dwell_anchor = start_ns.saturating_add(segment_duration_ns);
                    // 如果**本** tick 推得过远连 Dwell 都已经超期 →
                    // 直接推进到下一 waypoint（避免再发一帧 Dwell hold）。
                    if now_ns.saturating_sub(dwell_anchor) >= dwell_ns {
                        return Ok(self.advance_waypoint_and_emit(ctx, now_ns));
                    }
                    self.phase = Phase::Dwell {
                        start_ns: dwell_anchor,
                    };
                    1.0
                } else {
                    // 线性插值：tick_buf[j] = from[j] + α * (to[j] - from[j])
                    #[allow(clippy::cast_precision_loss)]
                    let alpha = (elapsed as f64) / (segment_duration_ns as f64);
                    // 取 from 切片时不要持 self 引用横跨 self.tick_buf 写
                    // —— inline 同款逻辑（segment 0 → initial_pose；i>0
                    // → decoded_waypoints[i-1].positions）。current_wp
                    // 已是 &self.decoded_waypoints[self.current_idx] 不能
                    // 跟 mut self.tick_buf 共存，所以这里不走 helper。
                    let from: &[f64] = if self.current_idx == 0 {
                        &self.initial_pose
                    } else {
                        &self.decoded_waypoints[self.current_idx - 1].positions
                    };
                    let to: &[f64] = &current_wp.positions;
                    self.tick_buf.clear();
                    for (f, t) in from.iter().zip(to.iter()) {
                        self.tick_buf.push(f + alpha * (t - f));
                    }
                    // segment_progress: Interp 阶段占 segment 时长
                    // 的比例（[0.0, 1.0)）；Dwell 阶段额外贡献 ε 让
                    // total progress 不卡在 (current_idx + 1) / total
                    // 之下；本轮简化为 alpha as f32（Dwell 占 segment
                    // 完成后才到 1.0）。
                    #[allow(clippy::cast_possible_truncation)]
                    let sp = alpha as f32;
                    sp.clamp(0.0, 1.0_f32 - f32::EPSILON)
                }
            }
            Phase::Dwell { start_ns } => {
                let elapsed = now_ns.saturating_sub(start_ns);
                // Dwell 期 target = waypoint.positions 不变。
                self.tick_buf.clear();
                self.tick_buf.extend_from_slice(&current_wp.positions);
                if elapsed >= dwell_ns {
                    // Dwell 完成 → advance waypoint（或 Completed）。
                    return Ok(self.advance_waypoint_and_emit(ctx, now_ns));
                }
                1.0
            }
        };

        // (4) build_hold_command 复用（B3 §3.7 source_task_id 单调契约：
        // source_task_id == ctx.command_seq，**不**用私有 counter）。
        let body = build_hold_command(
            ctx.config,
            ctx.layout_id,
            &self.tick_buf,
            /* source_task_id */ ctx.command_seq,
            /* now_ns */ now_ns,
        );

        Ok(ActionTickOutput::Continue {
            progress: self.progress(segment_progress),
            command: Some(JointCommandBody::new(body)),
        })
    }

    fn cancel(&mut self, _ctx: &mut ActionContext<'_>) -> Result<(), ActionCancelError> {
        // cooperative：set flag，下次 tick 自查收敛走 cancel_outcome。
        self.cancelled = true;
        Ok(())
    }
}

impl ScriptedInstance {
    /// 推进到下一 waypoint；末段完成则返 `Completed(Success)`。
    ///
    /// 中间段：重置 `phase = Interp { start_ns: now_ns }` + 构 hold cmd
    /// body（target = 刚完成的 waypoint.positions，保 cmd 输出连续不
    /// 跳变）返 `Continue`。
    ///
    /// 不返 `Result` 因本路径永远成功 ——`tick()` 调用方包 `Ok(...)`
    /// 投回 trait 签名。
    fn advance_waypoint_and_emit(
        &mut self,
        ctx: &mut ActionContext<'_>,
        now_ns: u64,
    ) -> ActionTickOutput {
        // 推进 idx：标记当前 waypoint 已完成。
        self.current_idx += 1;

        if self.current_idx >= self.decoded_waypoints.len() {
            // 末段 Dwell（或 dwell_ms == 0 时末段 Interp）完成 → Success。
            return ActionTickOutput::Completed(ActionOutcome::Success);
        }

        // 进入下一段 Interp。phase_started = true（已用真 now_ns 填）。
        self.phase = Phase::Interp { start_ns: now_ns };
        self.phase_started = true;

        // 构 hold cmd：本 tick target = 上一 waypoint 的终点（也是
        // 下一段的 from），保 cmd 输出连续不跳变。tick_buf 已在
        // 调用者那侧填好（上一 waypoint.positions），直接用。
        let body = build_hold_command(
            ctx.config,
            ctx.layout_id,
            &self.tick_buf,
            /* source_task_id */ ctx.command_seq,
            /* now_ns */ now_ns,
        );

        // segment_progress = 0.0（新 segment 起点）。
        ActionTickOutput::Continue {
            progress: self.progress(0.0),
            command: Some(JointCommandBody::new(body)),
        }
    }
}

// ============================================================================
// inventory 注册
// ============================================================================

/// inventory 静态实例。
///
/// `BackendRegistry::from_inventory` 启动期一次性扫描，把本实例的
/// `&'static dyn ActionBackend` 收进 `by_name` map（key = `"scripted"`）。
static SCRIPTED_BACKEND: ScriptedBackend = ScriptedBackend;

inventory::submit! {
    &SCRIPTED_BACKEND as &'static dyn ActionBackend
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    use crate::action::{ActionId, ActionKind, ActionRequest, ActionSource};
    use crate::coordinator::{CoordinatorConfig, HardwareSection};
    use flatbuffers::{FlatBufferBuilder, root};
    use robot_core::time::{MockClock, MonoRawNs};
    use robot_msg::msg::{
        ControlMode, JointCommand as FbJointCommand, ScriptedPayload as FbScriptedPayloadTable,
        ScriptedPayloadArgs, WaypointTarget, WaypointTargetArgs,
    };

    const TEST_LAYOUT_ID: u64 = 0xCAFE_BABE_DEAD_BEEF;

    // ----- helpers -----

    fn sample_hardware() -> HardwareSection {
        HardwareSection {
            id: "test_arm".into(),
            joints: vec!["j0".into(), "j1".into(), "j2".into()],
        }
    }

    /// 构造一个最小合法 `ScriptedPayload` fbs payload bytes。
    ///
    /// `waypoints` = `[(positions, dwell_ms, segment_duration_ms)]`。
    fn build_scripted_payload_bytes(waypoints: &[(Vec<f64>, u32, u32)]) -> Vec<u8> {
        let mut fbb = FlatBufferBuilder::with_capacity(256);
        let wp_offsets: Vec<_> = waypoints
            .iter()
            .map(|(positions, dwell_ms, segment_duration_ms)| {
                let positions_vec = fbb.create_vector::<f64>(positions);
                WaypointTarget::create(
                    &mut fbb,
                    &WaypointTargetArgs {
                        positions: Some(positions_vec),
                        dwell_ms: *dwell_ms,
                        segment_duration_ms: *segment_duration_ms,
                    },
                )
            })
            .collect();
        let wps_vec = fbb.create_vector(&wp_offsets);
        let payload = FbScriptedPayloadTable::create(
            &mut fbb,
            &ScriptedPayloadArgs {
                waypoints: Some(wps_vec),
            },
        );
        fbb.finish(payload, None);
        fbb.finished_data().to_vec()
    }

    /// 用给定 payload + clock_now_ns 构造 backend instance。
    fn fresh_instance_with_payload(
        payload: Vec<u8>,
        anchor_bias: Vec<f64>,
        clock_now_ns: u64,
    ) -> (
        Box<dyn ActionInstance>,
        CoordinatorConfig,
        HardwareSection,
        Vec<f64>,
        MockClock,
    ) {
        let backend = ScriptedBackend;
        let cfg = CoordinatorConfig::default();
        let hardware = sample_hardware();
        let clock = MockClock::new(MonoRawNs(clock_now_ns));
        let req = ActionRequest {
            action_id: ActionId::new_v7(),
            kind: ActionKind::Scripted,
            deadline_ns: u64::MAX,
            priority: 10,
            payload_schema_id: SCHEMA_SCRIPTED_PAYLOAD,
            payload,
            idempotency_key: "test-scripted-key".into(),
            source: ActionSource::Internal,
        };
        let ctx = ActionContext {
            clock: &clock,
            layout_id: TEST_LAYOUT_ID,
            hardware: &hardware,
            config: &cfg,
            anchor_bias: &anchor_bias,
            command_seq: 0,
            solver: None,
        };
        let instance = backend.start(&req, &ctx).expect("start ok");
        (instance, cfg, hardware, anchor_bias, clock)
    }

    // ----- 1. name() -----

    #[test]
    fn name_is_scripted() {
        let b = ScriptedBackend;
        assert_eq!(b.name(), "scripted");
    }

    // ----- 2. supported_kinds() -----

    #[test]
    fn supported_kinds_is_scripted_only() {
        let b = ScriptedBackend;
        let kinds = b.supported_kinds();
        assert_eq!(kinds.len(), 1);
        assert_eq!(kinds[0], ActionKind::Scripted);
    }

    // ----- 3. supported_schemas() -----

    #[test]
    fn supported_schemas_is_scripted_payload_only() {
        // **不**含 NO_PAYLOAD_SCHEMA_ID（跟 hold_pose / mock_dwell 显
        // 著区分）。
        let b = ScriptedBackend;
        let schemas = b.supported_schemas();
        assert_eq!(schemas, &[SCHEMA_SCRIPTED_PAYLOAD]);
    }

    // ----- 4. start_rejects_empty_waypoints -----

    #[test]
    fn start_rejects_empty_waypoints() {
        // empty waypoints → backend lib 返**裸** ActionStartError::Rejected
        // （reviewer Plan round 2 阻断 1 拍板：不是 SubmitError::
        // BackendStartFailed —— 后者是 coordinator submit 包装路径）。
        let backend = ScriptedBackend;
        let cfg = CoordinatorConfig::default();
        let hardware = sample_hardware();
        let anchor_bias = vec![0.0_f64; 3];
        let clock = MockClock::new(MonoRawNs(1_000_000_000));
        let payload = build_scripted_payload_bytes(&[]);
        let req = ActionRequest {
            action_id: ActionId::new_v7(),
            kind: ActionKind::Scripted,
            deadline_ns: u64::MAX,
            priority: 10,
            payload_schema_id: SCHEMA_SCRIPTED_PAYLOAD,
            payload,
            idempotency_key: "test-empty".into(),
            source: ActionSource::Internal,
        };
        let ctx = ActionContext {
            clock: &clock,
            layout_id: TEST_LAYOUT_ID,
            hardware: &hardware,
            config: &cfg,
            anchor_bias: &anchor_bias,
            command_seq: 0,
            solver: None,
        };
        let Err(err) = backend.start(&req, &ctx) else {
            panic!("expected Err");
        };
        assert!(
            matches!(err, ActionStartError::Rejected(ref s) if s.contains("non-empty")),
            "expected ActionStartError::Rejected(non-empty...), got {err:?}"
        );
    }

    // ----- 5. start_rejects_payload_schema_id_mismatch -----

    #[test]
    fn start_rejects_payload_schema_id_mismatch() {
        // payload_schema_id != SCHEMA_SCRIPTED_PAYLOAD → backend lib 返
        // **裸** ActionStartError::Rejected。
        let backend = ScriptedBackend;
        let cfg = CoordinatorConfig::default();
        let hardware = sample_hardware();
        let anchor_bias = vec![0.0_f64; 3];
        let clock = MockClock::new(MonoRawNs(1_000_000_000));
        let payload = build_scripted_payload_bytes(&[(vec![0.1, 0.2, 0.3], 100, 500)]);
        let req = ActionRequest {
            action_id: ActionId::new_v7(),
            kind: ActionKind::Scripted,
            deadline_ns: u64::MAX,
            priority: 10,
            payload_schema_id: 0xDEAD_BEEF, // 故意错的 schema_id
            payload,
            idempotency_key: "test-bad-schema".into(),
            source: ActionSource::Internal,
        };
        let ctx = ActionContext {
            clock: &clock,
            layout_id: TEST_LAYOUT_ID,
            hardware: &hardware,
            config: &cfg,
            anchor_bias: &anchor_bias,
            command_seq: 0,
            solver: None,
        };
        let Err(err) = backend.start(&req, &ctx) else {
            panic!("expected Err");
        };
        assert!(
            matches!(err, ActionStartError::Rejected(ref s) if s.contains("payload_schema_id")),
            "expected ActionStartError::Rejected(payload_schema_id...), got {err:?}"
        );
    }

    // ----- 5b. start_rejects_positions_length_mismatch -----

    #[test]
    fn start_rejects_positions_length_mismatch() {
        let backend = ScriptedBackend;
        let cfg = CoordinatorConfig::default();
        let hardware = sample_hardware(); // 3 joints
        let anchor_bias = vec![0.0_f64; 3];
        let clock = MockClock::new(MonoRawNs(1_000_000_000));
        // positions 长度 = 2，跟 hardware.joints.len() = 3 不匹配
        let payload = build_scripted_payload_bytes(&[(vec![0.1, 0.2], 100, 500)]);
        let req = ActionRequest {
            action_id: ActionId::new_v7(),
            kind: ActionKind::Scripted,
            deadline_ns: u64::MAX,
            priority: 10,
            payload_schema_id: SCHEMA_SCRIPTED_PAYLOAD,
            payload,
            idempotency_key: "test-joint-mismatch".into(),
            source: ActionSource::Internal,
        };
        let ctx = ActionContext {
            clock: &clock,
            layout_id: TEST_LAYOUT_ID,
            hardware: &hardware,
            config: &cfg,
            anchor_bias: &anchor_bias,
            command_seq: 0,
            solver: None,
        };
        let Err(err) = backend.start(&req, &ctx) else {
            panic!("expected Err");
        };
        assert!(
            matches!(err, ActionStartError::Rejected(ref s)
                if s.contains("positions length") && s.contains("hardware.joints.len()")),
            "got {err:?}"
        );
    }

    // ----- 5c. start_rejects_zero_segment_duration -----

    #[test]
    fn start_rejects_zero_segment_duration() {
        let backend = ScriptedBackend;
        let cfg = CoordinatorConfig::default();
        let hardware = sample_hardware();
        let anchor_bias = vec![0.0_f64; 3];
        let clock = MockClock::new(MonoRawNs(1_000_000_000));
        let payload = build_scripted_payload_bytes(&[(vec![0.1, 0.2, 0.3], 100, 0)]);
        let req = ActionRequest {
            action_id: ActionId::new_v7(),
            kind: ActionKind::Scripted,
            deadline_ns: u64::MAX,
            priority: 10,
            payload_schema_id: SCHEMA_SCRIPTED_PAYLOAD,
            payload,
            idempotency_key: "test-zero-seg".into(),
            source: ActionSource::Internal,
        };
        let ctx = ActionContext {
            clock: &clock,
            layout_id: TEST_LAYOUT_ID,
            hardware: &hardware,
            config: &cfg,
            anchor_bias: &anchor_bias,
            command_seq: 0,
            solver: None,
        };
        let Err(err) = backend.start(&req, &ctx) else {
            panic!("expected Err");
        };
        assert!(
            matches!(err, ActionStartError::Rejected(ref s) if s.contains("segment_duration_ms")),
            "got {err:?}"
        );
    }

    // ----- 6. tick_segment_returns_command_with_source_task_id_eq_ctx_command_seq -----

    #[test]
    fn tick_segment_returns_command_with_source_task_id_eq_ctx_command_seq() {
        // **bit-equivalent test** + 连续 N tick 跨 segment source_task_id
        // 严格单调（B3 §3.7 + reviewer Plan round 1 阻断 4 锁 progress
        // 是 f32 不是 Option<f32>）。
        let payload = build_scripted_payload_bytes(&[
            (vec![1.0, 1.0, 1.0], 0, 1000),
            (vec![2.0, 2.0, 2.0], 0, 1000),
            (vec![3.0, 3.0, 3.0], 0, 1000),
        ]);
        let anchor_bias = vec![0.0_f64, 0.0, 0.0];
        let now_ns: u64 = 1_000_000_000;
        let (mut instance, cfg, hardware, _anchor, clock) =
            fresh_instance_with_payload(payload, anchor_bias.clone(), now_ns);

        // 6 个 tick：每次 advance clock 100ms，命令 seq 严格单调。
        let mut last_seq: Option<u64> = None;
        for i in 0_u64..6 {
            let mut ctx = ActionContext {
                clock: &clock,
                layout_id: TEST_LAYOUT_ID,
                hardware: &hardware,
                config: &cfg,
                anchor_bias: &anchor_bias,
                command_seq: (i + 1) * 100, // 100, 200, 300, ...
                solver: None,
            };
            let out = instance.tick(&mut ctx).expect("tick ok");
            let body = match out {
                ActionTickOutput::Continue {
                    progress: _,
                    command: Some(body),
                } => body,
                ActionTickOutput::Continue { command: None, .. } => {
                    panic!("iter {i}: expected Some(body) for Continue, got None")
                }
                ActionTickOutput::Completed(_) => {
                    panic!("iter {i}: expected Continue, got Completed")
                }
            };

            // 解码 body，验 source_task_id == ctx.command_seq + 单调。
            let cmd = root::<FbJointCommand<'_>>(body.as_bytes()).expect("fbs verify");
            let seq = cmd.source_task_id();
            assert_eq!(
                seq,
                (i + 1) * 100,
                "iter {i}: source_task_id 必须 == ctx.command_seq"
            );
            if let Some(prev) = last_seq {
                assert!(
                    seq > prev,
                    "iter {i}: source_task_id 必须严格单调（prev={prev}, now={seq}）"
                );
            }
            last_seq = Some(seq);

            // ground truth：bit-equivalent to build_hold_command。
            let target: Vec<f64> = cmd.target().expect("target").iter().collect();
            let ground_truth = build_hold_command(
                &cfg,
                TEST_LAYOUT_ID,
                &target,
                /* source_task_id */ seq,
                /* now_ns */ now_ns,
            );
            assert_eq!(
                body.as_bytes(),
                ground_truth.as_slice(),
                "iter {i}: body bytes 必须 bit-equivalent to build_hold_command(...)"
            );
            assert_eq!(cmd.mode(), ControlMode::Position);
            assert_eq!(cmd.layout_id(), TEST_LAYOUT_ID);
        }
    }

    // ----- 7. tick_interp_phase_lerps_between_waypoints -----

    #[test]
    fn tick_interp_phase_lerps_between_waypoints() {
        // segment 0: initial_pose [0,0,0] → waypoint [10,10,10]
        // segment_duration_ms = 1000；α=0.5 时 target = [5,5,5]。
        let payload = build_scripted_payload_bytes(&[(vec![10.0, 10.0, 10.0], 0, 1000)]);
        let anchor_bias = vec![0.0_f64, 0.0, 0.0];
        let start_ns: u64 = 1_000_000_000;
        let (mut instance, cfg, hardware, _anchor, clock) =
            fresh_instance_with_payload(payload, anchor_bias.clone(), start_ns);

        // 第一帧：α = 0（刚启动，phase_started 之后 elapsed = 0）。
        let mut ctx = ActionContext {
            clock: &clock,
            layout_id: TEST_LAYOUT_ID,
            hardware: &hardware,
            config: &cfg,
            anchor_bias: &anchor_bias,
            command_seq: 1,
            solver: None,
        };
        let ActionTickOutput::Continue {
            command: Some(body),
            ..
        } = instance.tick(&mut ctx).expect("tick ok")
        else {
            panic!("alpha=0: expected Continue with Some(body)");
        };
        let cmd = root::<FbJointCommand<'_>>(body.as_bytes()).expect("fbs verify");
        let target: Vec<f64> = cmd.target().expect("target").iter().collect();
        for (i, v) in target.iter().enumerate() {
            assert!(v.abs() < 1e-9, "alpha=0 时 target[{i}] 应 = 0.0，got {v}");
        }

        // 第二帧：advance 500ms → α = 0.5 → target ≈ [5, 5, 5]。
        clock.advance(robot_core::time::DurationNs::from_nanos(500_000_000));
        let mut ctx = ActionContext {
            clock: &clock,
            layout_id: TEST_LAYOUT_ID,
            hardware: &hardware,
            config: &cfg,
            anchor_bias: &anchor_bias,
            command_seq: 2,
            solver: None,
        };
        let ActionTickOutput::Continue {
            command: Some(body),
            ..
        } = instance.tick(&mut ctx).expect("tick ok")
        else {
            panic!("alpha=0.5: expected Continue with Some(body)");
        };
        let cmd = root::<FbJointCommand<'_>>(body.as_bytes()).expect("fbs verify");
        let target: Vec<f64> = cmd.target().expect("target").iter().collect();
        for (i, v) in target.iter().enumerate() {
            assert!(
                (v - 5.0).abs() < 1e-6,
                "alpha=0.5 时 target[{i}] 应 ≈ 5.0，got {v}"
            );
        }
    }

    // ----- 8. tick_dwell_phase_holds_at_waypoint_target -----

    #[test]
    fn tick_dwell_phase_holds_at_waypoint_target() {
        // segment_duration_ms = 100ms（短 interp）+ dwell_ms = 500ms。
        // advance 200ms → Interp 完成 → Dwell；target = waypoint.positions
        // 不变（多 tick 都返同一 target）。
        let payload = build_scripted_payload_bytes(&[(vec![7.0, 7.0, 7.0], 500, 100)]);
        let anchor_bias = vec![0.0_f64; 3];
        let start_ns: u64 = 1_000_000_000;
        let (mut instance, cfg, hardware, _anchor, clock) =
            fresh_instance_with_payload(payload, anchor_bias.clone(), start_ns);

        // 第一帧：进入 phase_started + Interp α=0。
        let mut ctx = ActionContext {
            clock: &clock,
            layout_id: TEST_LAYOUT_ID,
            hardware: &hardware,
            config: &cfg,
            anchor_bias: &anchor_bias,
            command_seq: 1,
            solver: None,
        };
        let _ = instance.tick(&mut ctx).expect("tick ok");

        // 推 200ms → Interp 完成（segment_duration_ms=100）→ Dwell 起点。
        clock.advance(robot_core::time::DurationNs::from_nanos(200_000_000));
        let mut ctx = ActionContext {
            clock: &clock,
            layout_id: TEST_LAYOUT_ID,
            hardware: &hardware,
            config: &cfg,
            anchor_bias: &anchor_bias,
            command_seq: 2,
            solver: None,
        };
        let _ = instance.tick(&mut ctx).expect("tick ok");

        // Dwell 期：再 tick 几帧，target 应不变 = [7, 7, 7]。
        for i in 0_u64..3 {
            clock.advance(robot_core::time::DurationNs::from_nanos(50_000_000));
            let mut ctx = ActionContext {
                clock: &clock,
                layout_id: TEST_LAYOUT_ID,
                hardware: &hardware,
                config: &cfg,
                anchor_bias: &anchor_bias,
                command_seq: 10 + i,
                solver: None,
            };
            let ActionTickOutput::Continue {
                command: Some(body),
                ..
            } = instance.tick(&mut ctx).expect("tick ok")
            else {
                panic!("dwell iter {i}: expected Continue with Some(body)");
            };
            let cmd = root::<FbJointCommand<'_>>(body.as_bytes()).expect("fbs verify");
            let target: Vec<f64> = cmd.target().expect("target").iter().collect();
            for (j, v) in target.iter().enumerate() {
                assert!(
                    (v - 7.0).abs() < 1e-9,
                    "dwell iter {i}: target[{j}] 应 = 7.0（不变），got {v}"
                );
            }
        }
    }

    // ----- 9. tick_completed_after_last_dwell -----

    #[test]
    fn tick_completed_after_last_dwell() {
        // 单 waypoint，short interp + short dwell；推过 total duration
        // 后返 Completed(Success)。
        let payload = build_scripted_payload_bytes(&[(vec![1.0, 1.0, 1.0], 100, 100)]);
        let anchor_bias = vec![0.0_f64; 3];
        let start_ns: u64 = 1_000_000_000;
        let (mut instance, cfg, hardware, _anchor, clock) =
            fresh_instance_with_payload(payload, anchor_bias.clone(), start_ns);

        // Tick 1：phase_started + Interp α=0。
        let mut ctx = ActionContext {
            clock: &clock,
            layout_id: TEST_LAYOUT_ID,
            hardware: &hardware,
            config: &cfg,
            anchor_bias: &anchor_bias,
            command_seq: 1,
            solver: None,
        };
        let _ = instance.tick(&mut ctx).expect("tick ok");

        // 推 1000ms（> 100ms interp + 100ms dwell）→ 末段 Dwell 完 →
        // Completed(Success)。
        clock.advance(robot_core::time::DurationNs::from_nanos(1_000_000_000));
        let mut ctx = ActionContext {
            clock: &clock,
            layout_id: TEST_LAYOUT_ID,
            hardware: &hardware,
            config: &cfg,
            anchor_bias: &anchor_bias,
            command_seq: 2,
            solver: None,
        };
        let out = instance.tick(&mut ctx).expect("tick ok");
        match out {
            ActionTickOutput::Completed(ActionOutcome::Success) => {}
            other => panic!("expected Completed(Success), got {other:?}"),
        }
    }

    // ----- 10. tick_after_cancel_returns_completed_partial_success -----

    #[test]
    fn tick_after_cancel_returns_completed_partial_success() {
        // backend lib 层 cancel 收敛走 PartialSuccess（Q4(a) 拍板）。
        // coordinator pending_cancel 路径转 ActionEvent::Cancelled 是
        // coordinator 层职责，不在 lib unit 验证。
        let payload = build_scripted_payload_bytes(&[(vec![1.0, 1.0, 1.0], 1000, 1000)]);
        let anchor_bias = vec![0.0_f64; 3];
        let start_ns: u64 = 1_000_000_000;
        let (mut instance, cfg, hardware, _anchor, clock) =
            fresh_instance_with_payload(payload, anchor_bias.clone(), start_ns);

        // 先 tick 一帧（验状态确实在跑）。
        let mut ctx = ActionContext {
            clock: &clock,
            layout_id: TEST_LAYOUT_ID,
            hardware: &hardware,
            config: &cfg,
            anchor_bias: &anchor_bias,
            command_seq: 1,
            solver: None,
        };
        let out = instance.tick(&mut ctx).expect("tick ok");
        assert!(matches!(out, ActionTickOutput::Continue { .. }));

        // cancel + 下次 tick → PartialSuccess { warning:
        // "cancelled_at_waypoint_N" }。current_idx 在 cancel 时是 0
        // （第一段还没完成）。
        instance.cancel(&mut ctx).expect("cancel ok");
        let out = instance.tick(&mut ctx).expect("tick ok");
        match out {
            ActionTickOutput::Completed(ActionOutcome::PartialSuccess { warning }) => {
                assert_eq!(warning, "cancelled_at_waypoint_0");
            }
            other => panic!("expected Completed(PartialSuccess), got {other:?}"),
        }
    }

    // ----- 11. inventory_default_instance_present -----

    #[test]
    fn inventory_default_instance_present() {
        // 锁住 ScriptedBackend 静态实例真注册到 inventory；改 inventory
        // 注册口要同步改这条测试。沿 hold_pose 同款 + Q15(a) 拍板。
        let names: Vec<&'static str> = inventory::iter::<&'static dyn ActionBackend>
            .into_iter()
            .map(|b| b.name())
            .collect();
        assert!(
            names.contains(&"scripted"),
            "ScriptedBackend 必须通过 inventory::submit! 真注册；inventory 名字集 = {names:?}"
        );
        // 同时验 hold_pose 也在（Q15 拍板：names 集断言含两个）。
        assert!(
            names.contains(&"hold_pose"),
            "hold_pose 仍应在 inventory；names = {names:?}"
        );
    }
}
