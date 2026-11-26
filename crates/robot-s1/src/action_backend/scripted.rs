//! Scripted 动作后端：关节位置插值、驻留与单关节实测到位确认。
//!
//! 同一 `scripted` profile 接受完整向量、单关节定位与所选侧手臂固定回位。
//! `ScriptedPayload` 保留完整向量、
//! 多 waypoint、末目标发布后完成的行为；`SingleJointPositionPayload` 仅替换
//! 提交时新鲜完整快照中的一个目标，受显式运动配置、任务期限和反馈判据约束。
//! 回位从同一帧取五轴起点及其余位置轴的保持值，共用插值实例和反馈到位判据。
//!
//! 新单关节请求只选择位置关节，混合总线三轮目标保持 raw0。末目标发布成功后
//! 继续保持末目标，只有其后新鲜反馈中的全部位置关节连续满足各自容差及稳定
//! 窗口才报告成功。取消先于反馈
//! 检查、总期限先于取消；中断保持使用最后成功发布的完整目标，首次尚未发布
//! 则使用提交快照。这不表示当前位置保持或断扭。旧全向量请求的取消/失败保持
//! 语义继续由 coordinator 原分支决定。
//!
//! 每条命令沿用 coordinator 的 `command_seq`，起点沿用 `scripted_start_pose`，
//! 不回退启动锚点；混合总线请求继续保持三轮 raw0。tick 复用位置缓冲，
//! 后端不创建线程、不直接访问硬件或发布 IPC。

#![allow(clippy::doc_markdown)]

use crate::action::{
    ActionBackend, ActionCancelError, ActionContext, ActionFailure, ActionInstance, ActionKind,
    ActionOutcome, ActionRequest, ActionStartError, ActionTickOutput, JointCommandBody,
};
use crate::coordinator::build_hardware_hold_command;
#[cfg(test)]
use crate::coordinator::build_hold_command;
use crate::single_joint::{SingleJointExecution, SingleJointPlan, position_for_s0};
use robot_msg::msg::ScriptedPayload as FbScriptedPayload;
use robot_msg::schema_id::{
    SCHEMA_LEFT_ARM_HOME_PAYLOAD, SCHEMA_RIGHT_ARM_HOME_PAYLOAD, SCHEMA_SCRIPTED_PAYLOAD,
    SCHEMA_SINGLE_JOINT_POSITION_PAYLOAD,
};

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
        const SCHEMAS: &[u32] = &[
            SCHEMA_SCRIPTED_PAYLOAD,
            SCHEMA_SINGLE_JOINT_POSITION_PAYLOAD,
            SCHEMA_RIGHT_ARM_HOME_PAYLOAD,
            SCHEMA_LEFT_ARM_HOME_PAYLOAD,
        ];
        SCHEMAS
    }

    fn start(
        &self,
        req: &ActionRequest,
        ctx: &ActionContext<'_>,
    ) -> Result<Box<dyn ActionInstance>, ActionStartError> {
        if req.payload_schema_id == SCHEMA_SINGLE_JOINT_POSITION_PAYLOAD {
            return Self::start_single_joint(req, ctx);
        }
        if matches!(
            req.payload_schema_id,
            SCHEMA_LEFT_ARM_HOME_PAYLOAD | SCHEMA_RIGHT_ARM_HOME_PAYLOAD
        ) {
            return SingleJointPlan::arm_home(req, ctx).map(Self::position_instance);
        }
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

        // (4) 每段位置维度匹配、有限且可表示为 S0 的 f32，时长 > 0。
        // decode 期一次性 owned 拷贝到
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
            if let Some((joint, value)) = positions
                .iter()
                .enumerate()
                .find(|(_, value)| !value.is_finite() || value.abs() > f64::from(f32::MAX))
            {
                return Err(ActionStartError::Rejected(format!(
                    "ScriptedPayload.waypoints[{idx}].positions[{joint}] must be finite and fit f32, got {value}"
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

        // (5) segment 0 from = snapshot 自 ctx.scripted_start_pose（v0.5
        // 候选 E 段(5) D3 第 1 条：提交时当前位，coordinator 自常驻 state
        // cache 取；缺失 = coordinator 未取到 / 路由漂移 → 拒绝，**绝不**
        // fallback 到 ctx.anchor_bias——静默跳变危险）。
        let Some(start_pose) = ctx.scripted_start_pose else {
            return Err(ActionStartError::Rejected(
                "ScriptedBackend: ctx.scripted_start_pose missing (submit-time current \
                 position unavailable; D3: no fallback to anchor_bias)"
                    .into(),
            ));
        };
        // 长度必须跟 hardware.joints.len() 一致（cache latest_fresh 已校
        // joint_count；此处 defensive 断言）。
        if start_pose.len() != expected_joints {
            return Err(ActionStartError::Rejected(format!(
                "ScriptedBackend: ctx.scripted_start_pose length {} != hardware.joints.len() {expected_joints}",
                start_pose.len()
            )));
        }
        if let Some((joint, value)) = start_pose
            .iter()
            .enumerate()
            .find(|(_, value)| !value.is_finite() || value.abs() > f64::from(f32::MAX))
        {
            return Err(ActionStartError::Rejected(format!(
                "ScriptedBackend: ctx.scripted_start_pose[{joint}] must be finite and fit f32, got {value}"
            )));
        }
        let initial_pose: Vec<f64> = start_pose.to_vec();

        let instance = ScriptedInstance {
            cancelled: false,
            decoded_waypoints,
            initial_pose,
            current_idx: 0,
            phase: Phase::Interp { start_ns: 0 },
            phase_started: false,
            final_published: false,
            tick_buf: Vec::with_capacity(expected_joints),
            single_joint: None,
        };
        Ok(Box::new(instance))
    }
}

impl ScriptedBackend {
    fn start_single_joint(
        req: &ActionRequest,
        ctx: &ActionContext<'_>,
    ) -> Result<Box<dyn ActionInstance>, ActionStartError> {
        let plan = SingleJointPlan::prepare(req, ctx)?;
        Ok(Self::position_instance(plan))
    }

    #[cfg(feature = "backend-ik")]
    pub(super) fn start_ik_joints(
        req: &ActionRequest,
        ctx: &ActionContext<'_>,
        targets: &[(usize, f64)],
    ) -> Result<Box<dyn ActionInstance>, ActionStartError> {
        let config = ctx.single_joint_config.ok_or_else(|| {
            ActionStartError::Rejected("混合总线 IK 需要配置位置约束和到位容差".into())
        })?;
        let plan =
            SingleJointPlan::prepare_positions(req, ctx, config, config.max_duration_ms, targets)?;
        Ok(Self::position_instance(plan))
    }

    fn position_instance(plan: SingleJointPlan) -> Box<dyn ActionInstance> {
        // prepare 已检查同一快照、完整维度、配置范围与硬件状态。
        let initial_pose = plan.initial_pose;
        let tick_buf = Vec::with_capacity(initial_pose.len());
        Box::new(ScriptedInstance {
            cancelled: false,
            decoded_waypoints: vec![DecodedWaypoint {
                positions: plan.positions,
                dwell_ms: 0,
                segment_duration_ms: plan.duration_ms,
            }],
            initial_pose,
            current_idx: 0,
            phase: Phase::Interp { start_ns: 0 },
            phase_started: false,
            final_published: false,
            tick_buf,
            single_joint: Some(plan.execution),
        })
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
/// 段）；末段完成后若末 target 尚未发布，先进 `FinalEmitted`（本 tick 发
/// 末 target 帧）。旧请求下一 tick 完成，新单关节请求等待实测到位。
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
    /// 末 waypoint 已完成且末 target 帧已于上一 tick 返回（v0.5 候选 E
    /// 段(5)）；旧请求下一 tick 完成，新请求继续末目标保持及反馈确认，
    /// 不再索引 `decoded_waypoints[current_idx]`（此时 `current_idx == len`）。
    FinalEmitted,
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
    /// segment 0 的 `from`（snapshot 自 `ctx.scripted_start_pose`——提交时
    /// 当前位，v0.5 候选 E 段(5) D3 第 1 条；v0.4 原为 `ctx.anchor_bias`）。
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
    /// 末 waypoint target 是否已作为一帧 `Continue { command: Some }` 返给
    /// coordinator（v0.5 候选 E 段(5) 末点发布保证）。置位路径：末
    /// waypoint 的 Dwell 帧（含 Interp→Dwell 过渡帧）/ `FinalEmitted` 帧。
    /// `successful_hold_target()` 仅在本 flag 为 true 时返 `Some`。
    final_published: bool,
    /// tick 期目标位置缓冲（`with_capacity(hardware.joints.len())` 预
    /// 分配，tick 期 `clear()` + extend 复用；Q7 拍板避 per-tick alloc）。
    tick_buf: Vec<f64>,
    /// 新单关节请求专有的约束与反馈状态；旧载荷恒为 None。
    single_joint: Option<SingleJointExecution>,
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

    /// 当前 waypoint 是否为末 waypoint（`start()` 守门 waypoints 非空）。
    fn on_last_waypoint(&self) -> bool {
        self.current_idx + 1 == self.decoded_waypoints.len()
    }

    /// 末 waypoint positions（`start()` 守门 waypoints 非空 → 必有）。
    fn last_waypoint_positions(&self) -> &[f64] {
        self.decoded_waypoints
            .last()
            .map_or(&[], |wp| wp.positions.as_slice())
    }

    /// 插值与到位确认阶段共用当前目标缓冲及同一命令序号。
    fn emit_current_target(
        &mut self,
        ctx: &ActionContext<'_>,
        now_ns: u64,
        progress: f32,
    ) -> ActionTickOutput {
        if self.single_joint.is_some() {
            // 与 S0 实际位置表示一致；已验证端点的线性插值不以 clamp 修正。
            for position in &mut self.tick_buf {
                *position = position_for_s0(*position);
            }
        }
        let body = build_hardware_hold_command(
            ctx.config,
            ctx.hardware,
            ctx.layout_id,
            &self.tick_buf,
            ctx.command_seq,
            now_ns,
        );
        ActionTickOutput::Continue {
            progress,
            command: Some(JointCommandBody::new(body)),
        }
    }
}

impl ActionInstance for ScriptedInstance {
    fn tick(&mut self, ctx: &mut ActionContext<'_>) -> Result<ActionTickOutput, ActionFailure> {
        let now_ns = ctx.clock.now().0;
        if let Some(single_joint) = self.single_joint.as_ref() {
            single_joint.check_deadline(now_ns)?;
        }
        // (1) cancel 短路（Q4(a)）：backend lib 返 PartialSuccess；
        // coordinator pending_cancel 路径会转成 ActionEvent::Cancelled
        // 写事件流。
        if self.cancelled {
            return Ok(ActionTickOutput::Completed(self.cancel_outcome()));
        }

        if let Some(single_joint) = self.single_joint.as_mut() {
            let confirmed = single_joint.observe_feedback(now_ns, ctx.feedback)?;
            if matches!(self.phase, Phase::FinalEmitted) {
                if confirmed {
                    return Ok(ActionTickOutput::Completed(ActionOutcome::Success));
                }
                // 等待实测到位时持续发末目标；没有发布回调也不能提前成功。
                return Ok(self.emit_current_target(ctx, now_ns, 1.0));
            }
        }

        // (1b) 末点发布保证收敛（v0.5 候选 E 段(5)）：上一 tick 已返末
        // target 帧（FinalEmitted）→ 本 tick 返 Completed(Success)。必须
        // 先于下面的 decoded_waypoints[current_idx] 索引（此时 idx == len）。
        if matches!(self.phase, Phase::FinalEmitted) {
            debug_assert!(self.final_published, "FinalEmitted 必伴随 final_published");
            return Ok(ActionTickOutput::Completed(ActionOutcome::Success));
        }

        // (2) phase start_ns 延迟初始化（首次 tick 拿真 clock 填）。
        if !self.phase_started {
            self.phase = match self.phase {
                Phase::Interp { .. } => Phase::Interp { start_ns: now_ns },
                Phase::Dwell { .. } => Phase::Dwell { start_ns: now_ns },
                Phase::FinalEmitted => Phase::FinalEmitted,
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
                // Interp→Dwell 时已将终点写入 tick_buf，驻留期间直接复用。
                if elapsed >= dwell_ns {
                    // Dwell 完成 → advance waypoint（或 Completed）。
                    return Ok(self.advance_waypoint_and_emit(ctx, now_ns));
                }
                1.0
            }
            Phase::FinalEmitted => {
                // (1b) 已提前返回；此臂仅为 match 穷尽（不可达）。
                return Ok(ActionTickOutput::Completed(ActionOutcome::Success));
            }
        };

        // (3b) 末点发布保证（v0.5 候选 E 段(5)）：本帧处于末 waypoint 的
        // Dwell（含刚从 Interp 过渡进 Dwell 的这一帧）→ target 即末
        // waypoint → 记"末 target 已发布"；dwell 完成后 advance 可直接
        // Completed（dwell>0 路径行为不变，无额外帧）。
        if self.on_last_waypoint() && matches!(self.phase, Phase::Dwell { .. }) {
            self.final_published = true;
        }

        // (4) build_hold_command 复用（B3 §3.7 source_task_id 单调契约：
        // source_task_id == ctx.command_seq，**不**用私有 counter）。
        Ok(self.emit_current_target(ctx, now_ns, self.progress(segment_progress)))
    }

    fn cancel(&mut self, _ctx: &mut ActionContext<'_>) -> Result<(), ActionCancelError> {
        // cooperative：set flag，下次 tick 自查收敛走 cancel_outcome。
        self.cancelled = true;
        Ok(())
    }

    /// 成功后的末目标；新请求还要求 publisher 明确回报末帧成功。
    fn successful_hold_target(&self) -> Option<Vec<f64>> {
        (self.final_published
            && self
                .single_joint
                .as_ref()
                .is_none_or(SingleJointExecution::final_target_published))
        .then(|| self.last_waypoint_positions().to_vec())
    }

    fn command_published(&mut self, published_at_ns: u64) {
        if let Some(single_joint) = self.single_joint.as_mut() {
            single_joint.command_published(&self.tick_buf, self.final_published, published_at_ns);
        }
    }

    fn interrupted_hold_target(&self) -> Option<Vec<f64>> {
        self.single_joint
            .as_ref()
            .map(SingleJointExecution::interrupted_hold_target)
    }
}

impl ScriptedInstance {
    /// 推进到下一 waypoint；末段完成则按**末点发布保证**收敛。
    ///
    /// 中间段：重置 `phase = Interp { start_ns: now_ns }` + 构 hold cmd
    /// body（target = 刚完成的 waypoint.positions，保 cmd 输出连续不
    /// 跳变）返 `Continue`。
    ///
    /// 末段（`current_idx == len`）：
    /// - `final_published == true`（Dwell 期已发末 target）→ 直接
    ///   `Completed(Success)`（dwell>0 正常节奏路径，行为不变）。
    /// - `final_published == false`（dwell=0 / 大步推 clock 跳过 Dwell）→
    ///   返一帧 `Continue { target = 末 waypoint, progress = 1.0 }` 并进
    ///   `FinalEmitted`；下一 tick 才 `Completed(Success)`。
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
            if self.final_published {
                // 末 target 已在 Dwell 期发布 → Success（行为不变）。
                return ActionTickOutput::Completed(ActionOutcome::Success);
            }
            // 末 target 从未上线（dwell=0 / 大步跳过 Dwell）→ 先发一帧
            // Continue（tick_buf 已由调用方填成末 waypoint.positions），
            // 下一 tick 才 Completed（Spec D3 第 2 条）。
            debug_assert_eq!(
                self.tick_buf.as_slice(),
                self.last_waypoint_positions(),
                "末帧 tick_buf 必须 == 末 waypoint positions"
            );
            self.final_published = true;
            self.phase = Phase::FinalEmitted;
            self.phase_started = true;
            let body = build_hardware_hold_command(
                ctx.config,
                ctx.hardware,
                ctx.layout_id,
                &self.tick_buf,
                /* source_task_id */ ctx.command_seq,
                /* now_ns */ now_ns,
            );
            // current_idx == len → progress(0.0) == 1.0（末帧 progress 封顶）。
            return ActionTickOutput::Continue {
                progress: self.progress(0.0),
                command: Some(JointCommandBody::new(body)),
            };
        }

        // 进入下一段 Interp。phase_started = true（已用真 now_ns 填）。
        self.phase = Phase::Interp { start_ns: now_ns };
        self.phase_started = true;

        // 构 hold cmd：本 tick target = 上一 waypoint 的终点（也是
        // 下一段的 from），保 cmd 输出连续不跳变。tick_buf 已在
        // 调用者那侧填好（上一 waypoint.positions），直接用。
        let body = build_hardware_hold_command(
            ctx.config,
            ctx.hardware,
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
// 测试支撑（crate 内共享：本模块单测 + coordinator 单测复用同一 payload builder）
// ============================================================================

#[cfg(test)]
pub(crate) mod test_support {
    use flatbuffers::FlatBufferBuilder;
    use robot_msg::msg::{
        ScriptedPayload as FbScriptedPayloadTable, ScriptedPayloadArgs, WaypointTarget,
        WaypointTargetArgs,
    };

    /// 构造一个最小合法 `ScriptedPayload` fbs payload bytes。
    ///
    /// `waypoints` = `[(positions, dwell_ms, segment_duration_ms)]`。
    pub(crate) fn build_scripted_payload_bytes(waypoints: &[(Vec<f64>, u32, u32)]) -> Vec<u8> {
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
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::test_support::build_scripted_payload_bytes;
    use super::*;

    use crate::action::{ActionId, ActionKind, ActionRequest, ActionSource};
    use crate::coordinator::{CoordinatorConfig, HardwareSection};
    use flatbuffers::root;
    use robot_core::time::{MockClock, MonoRawNs};
    use robot_msg::msg::{ControlMode, JointCommand as FbJointCommand};

    const TEST_LAYOUT_ID: u64 = 0xCAFE_BABE_DEAD_BEEF;

    // ----- helpers -----

    fn sample_hardware() -> HardwareSection {
        HardwareSection {
            id: "test_arm".into(),
            joints: vec!["j0".into(), "j1".into(), "j2".into()],
            motor_ids: vec![1, 2, 3],
            feetech_bus2_mixed_raw: false,
            calibration_range_tolerances_raw: None,
        }
    }

    /// `anchor_bias` 哨兵：与任何测试 start_pose 都不同——证明 segment0
    /// from 来自 `scripted_start_pose` 而非 `anchor_bias`（D3 第 1 条）。
    const ANCHOR_SENTINEL: f64 = 99.0;

    /// 用给定 payload + `start_pose`（作 `ctx.scripted_start_pose`）+
    /// clock_now_ns 构造 backend instance。返回的第 4 项是 tick 期 ctx 用的
    /// `anchor_bias`（全 [`ANCHOR_SENTINEL`]，与 start_pose 无关）。
    fn fresh_instance_with_payload(
        payload: Vec<u8>,
        start_pose: &[f64],
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
        let anchor_bias = vec![ANCHOR_SENTINEL; hardware.joints.len()];
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
            scripted_start_pose: Some(start_pose),
            solver: None,
            single_joint_config: None,
            feedback: None,
        };
        let instance = backend.start(&req, &ctx).expect("start ok");
        (instance, cfg, hardware, anchor_bias, clock)
    }

    // ----- 1. name() -----

    #[test]
    fn mixed_scripted_all_emission_phases_keep_wheel_raw_zero() {
        let mut hardware = crate::coordinator::parse_hardware_section_from_toml(
            include_str!("../../../../configs/xlerobot-bus2.toml"),
            "right_arm_full_with_base",
        )
        .unwrap();
        hardware.feetech_bus2_mixed_raw = true;
        hardware.joints.reverse();
        hardware.motor_ids.reverse();
        let waypoint: Vec<_> = hardware
            .motor_ids
            .iter()
            .map(|&id| if id <= 6 { 0.5 } else { 500.0 })
            .collect();
        let req = ActionRequest {
            action_id: ActionId::new_v7(),
            kind: ActionKind::Scripted,
            deadline_ns: u64::MAX,
            priority: 10,
            payload_schema_id: SCHEMA_SCRIPTED_PAYLOAD,
            payload: build_scripted_payload_bytes(&[
                (waypoint.clone(), 0, 100),
                (waypoint, 0, 100),
            ]),
            idempotency_key: "mixed-scripted".into(),
            source: ActionSource::Internal,
        };
        let anchor = [0.0; 9];
        let clock = MockClock::new(MonoRawNs(1_000_000_000));
        let cfg = CoordinatorConfig::default();
        let mut ctx = ActionContext {
            clock: &clock,
            layout_id: TEST_LAYOUT_ID,
            hardware: &hardware,
            config: &cfg,
            anchor_bias: &anchor,
            command_seq: 0,
            scripted_start_pose: Some(&anchor),
            solver: None,
            single_joint_config: None,
            feedback: None,
        };
        let mut instance = ScriptedBackend.start(&req, &ctx).unwrap();
        for (seq, elapsed_ms) in [0_u64, 50, 100, 150, 200].into_iter().enumerate() {
            clock.set(MonoRawNs(1_000_000_000 + elapsed_ms * 1_000_000));
            ctx.command_seq = u64::try_from(seq).unwrap();
            let ActionTickOutput::Continue {
                command: Some(body),
                ..
            } = instance.tick(&mut ctx).unwrap()
            else {
                panic!("需要插值/段界/末帧命令")
            };
            let command = root::<FbJointCommand>(body.as_bytes()).unwrap();
            assert_eq!(command.mode(), ControlMode::MixedPositionRawVelocity);
            assert_eq!(
                command.raw_velocities().unwrap().iter().collect::<Vec<_>>(),
                [0; 9]
            );
            for (slot, id) in hardware.motor_ids.iter().enumerate() {
                if *id >= 7 {
                    assert_eq!(command.target().unwrap().get(slot), 0.0);
                }
            }
            assert_eq!(command.source_task_id(), ctx.command_seq);
        }
    }

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
    fn supported_schemas_include_legacy_and_single_joint() {
        // **不**含 NO_PAYLOAD_SCHEMA_ID（跟 hold_pose / mock_dwell 显
        // 著区分）。
        let b = ScriptedBackend;
        let schemas = b.supported_schemas();
        assert_eq!(
            schemas,
            &[
                SCHEMA_SCRIPTED_PAYLOAD,
                SCHEMA_SINGLE_JOINT_POSITION_PAYLOAD,
                SCHEMA_RIGHT_ARM_HOME_PAYLOAD,
                SCHEMA_LEFT_ARM_HOME_PAYLOAD,
            ]
        );
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
            scripted_start_pose: Some(&anchor_bias),
            solver: None,
            single_joint_config: None,
            feedback: None,
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
            scripted_start_pose: Some(&anchor_bias),
            solver: None,
            single_joint_config: None,
            feedback: None,
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
            scripted_start_pose: Some(&anchor_bias),
            solver: None,
            single_joint_config: None,
            feedback: None,
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

    fn assert_position_rejected(target: f64, start: f64, field: &str) {
        let backend = ScriptedBackend;
        let cfg = CoordinatorConfig::default();
        let hardware = sample_hardware();
        let anchor_bias = [0.0; 3];
        let start_pose = [0.0, start, 0.0];
        let clock = MockClock::new(MonoRawNs(1_000_000_000));
        let req = ActionRequest {
            action_id: ActionId::new_v7(),
            kind: ActionKind::Scripted,
            deadline_ns: u64::MAX,
            priority: 10,
            payload_schema_id: SCHEMA_SCRIPTED_PAYLOAD,
            // 第一段合法，第二段的错误也必须在生成任何实例前拒绝。
            payload: build_scripted_payload_bytes(&[
                (vec![0.0; 3], 0, 1),
                (vec![0.0, target, 0.0], 10, 1),
            ]),
            idempotency_key: "test-invalid-position".into(),
            source: ActionSource::Internal,
        };
        let ctx = ActionContext {
            clock: &clock,
            layout_id: TEST_LAYOUT_ID,
            hardware: &hardware,
            config: &cfg,
            anchor_bias: &anchor_bias,
            command_seq: 0,
            scripted_start_pose: Some(&start_pose),
            solver: None,
            single_joint_config: None,
            feedback: None,
        };
        let Err(error) = backend.start(&req, &ctx) else {
            panic!("非法位置不能创建可运行或保存保持目标的实例");
        };
        assert!(
            matches!(error, ActionStartError::Rejected(ref reason)
                if reason.contains(field) && reason.contains("finite and fit f32")),
            "拒绝应保留错误位置及原因：{error:?}"
        );
    }

    #[test]
    fn start_rejects_nonfinite_and_unrepresentable_waypoint_positions() {
        for invalid in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::MAX,
            -f64::MAX,
        ] {
            assert_position_rejected(invalid, 0.0, "waypoints[1].positions[1]");
        }
    }

    #[test]
    fn start_rejects_nonfinite_and_unrepresentable_start_positions() {
        for invalid in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::MAX,
            -f64::MAX,
        ] {
            assert_position_rejected(0.0, invalid, "scripted_start_pose[1]");
        }
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
            scripted_start_pose: Some(&anchor_bias),
            solver: None,
            single_joint_config: None,
            feedback: None,
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
            fresh_instance_with_payload(payload, &anchor_bias, now_ns);

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
                scripted_start_pose: None,
                solver: None,
                single_joint_config: None,
                feedback: None,
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
            fresh_instance_with_payload(payload, &anchor_bias, start_ns);

        // 第一帧：α = 0（刚启动，phase_started 之后 elapsed = 0）。
        let mut ctx = ActionContext {
            clock: &clock,
            layout_id: TEST_LAYOUT_ID,
            hardware: &hardware,
            config: &cfg,
            anchor_bias: &anchor_bias,
            command_seq: 1,
            scripted_start_pose: None,
            solver: None,
            single_joint_config: None,
            feedback: None,
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
            scripted_start_pose: None,
            solver: None,
            single_joint_config: None,
            feedback: None,
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
            fresh_instance_with_payload(payload, &anchor_bias, start_ns);

        // 第一帧：进入 phase_started + Interp α=0。
        let mut ctx = ActionContext {
            clock: &clock,
            layout_id: TEST_LAYOUT_ID,
            hardware: &hardware,
            config: &cfg,
            anchor_bias: &anchor_bias,
            command_seq: 1,
            scripted_start_pose: None,
            solver: None,
            single_joint_config: None,
            feedback: None,
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
            scripted_start_pose: None,
            solver: None,
            single_joint_config: None,
            feedback: None,
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
                scripted_start_pose: None,
                solver: None,
                single_joint_config: None,
                feedback: None,
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

    #[test]
    fn dwell_reuses_current_waypoint_across_segment_transition() {
        let first = [2.0, 4.0, 6.0];
        let second = [-1.0, -3.0, -5.0];
        let payload = build_scripted_payload_bytes(&[
            (first.to_vec(), 200, 100),
            (second.to_vec(), 100, 100),
        ]);
        let start_ns = 1_000_000_000;
        let (mut instance, cfg, hardware, anchor, clock) =
            fresh_instance_with_payload(payload, &[0.0; 3], start_ns);
        // 第一段驻留、切段、插值以及第二段驻留使用不同目标，防止复用旧段数据。
        for (seq, (elapsed_ms, expected)) in [
            (0_u64, [0.0; 3]),
            (100, first),
            (150, first),
            (299, first),
            (300, first),
            (350, [0.5; 3]),
            (400, second),
            (450, second),
        ]
        .into_iter()
        .enumerate()
        {
            let now_ns = start_ns + elapsed_ms * 1_000_000;
            clock.set(MonoRawNs(now_ns));
            let seq = u64::try_from(seq).unwrap() + 10;
            let mut ctx = tick_ctx(&clock, &hardware, &cfg, &anchor, seq);
            let ActionTickOutput::Continue {
                command: Some(body),
                ..
            } = instance.tick(&mut ctx).unwrap()
            else {
                panic!("t={elapsed_ms}ms 应继续发布目标");
            };
            let cmd = root::<FbJointCommand<'_>>(body.as_bytes()).unwrap();
            assert_eq!(decode_target(&body), expected);
            assert_eq!(cmd.source_task_id(), seq);
            assert_eq!(cmd.deadline_ns(), now_ns + cfg.deadline_offset_ns());
        }
        clock.set(MonoRawNs(start_ns + 500_000_000));
        let mut ctx = tick_ctx(&clock, &hardware, &cfg, &anchor, 18);
        assert!(matches!(
            instance.tick(&mut ctx).unwrap(),
            ActionTickOutput::Completed(ActionOutcome::Success)
        ));
        assert_eq!(instance.successful_hold_target(), Some(second.to_vec()));
    }

    // ----- 9. tick_completed_after_last_dwell（v0.5 候选 E 段(5) 末点发布
    //          保证改写：大步推 clock 跳过 Dwell → 先末帧 Continue 后 Completed）-----

    #[test]
    fn tick_completed_after_last_dwell_via_final_frame() {
        // 单 waypoint，short interp + short dwell；一帧推过 total duration：
        // v0.4 现状直接 Completed（末 target 从未上线）；段(5) 起先返一帧
        // Continue{target=末 waypoint, progress=1.0}，下一 tick 才 Completed。
        let payload = build_scripted_payload_bytes(&[(vec![1.0, 1.0, 1.0], 100, 100)]);
        let start_pose = vec![0.0_f64; 3];
        let start_ns: u64 = 1_000_000_000;
        let (mut instance, cfg, hardware, anchor_bias, clock) =
            fresh_instance_with_payload(payload, &start_pose, start_ns);

        // Tick 1：phase_started + Interp α=0。
        let mut ctx = ActionContext {
            clock: &clock,
            layout_id: TEST_LAYOUT_ID,
            hardware: &hardware,
            config: &cfg,
            anchor_bias: &anchor_bias,
            scripted_start_pose: None,
            command_seq: 1,
            solver: None,
            single_joint_config: None,
            feedback: None,
        };
        let _ = instance.tick(&mut ctx).expect("tick ok");
        assert!(
            instance.successful_hold_target().is_none(),
            "末帧未发布前 None"
        );

        // 推 1000ms（> 100ms interp + 100ms dwell）→ 末段 Dwell 被整体跳过
        // → 末 target 尚未发布 → 本 tick 必须先返 Continue(末 waypoint)。
        clock.advance(robot_core::time::DurationNs::from_nanos(1_000_000_000));
        let mut ctx = ActionContext {
            clock: &clock,
            layout_id: TEST_LAYOUT_ID,
            hardware: &hardware,
            config: &cfg,
            anchor_bias: &anchor_bias,
            scripted_start_pose: None,
            command_seq: 2,
            solver: None,
            single_joint_config: None,
            feedback: None,
        };
        let out = instance.tick(&mut ctx).expect("tick ok");
        let ActionTickOutput::Continue {
            progress,
            command: Some(body),
        } = out
        else {
            panic!("末 waypoint 完成帧必须是 Continue(Some(body))，got {out:?}");
        };
        assert!(
            (progress - 1.0).abs() < f32::EPSILON,
            "末帧 progress == 1.0，got {progress}"
        );
        let cmd = root::<FbJointCommand<'_>>(body.as_bytes()).expect("fbs verify");
        let target: Vec<f64> = cmd.target().expect("target").iter().collect();
        assert_eq!(target, vec![1.0, 1.0, 1.0], "末帧 target == 末 waypoint");
        assert_eq!(
            cmd.source_task_id(),
            2,
            "末帧 source_task_id == ctx.command_seq"
        );
        assert_eq!(
            instance.successful_hold_target(),
            Some(vec![1.0, 1.0, 1.0]),
            "末帧返回后 successful_hold_target == Some(末 waypoint)"
        );

        // Tick 3：Completed(Success)（不再产帧）。
        clock.advance(robot_core::time::DurationNs::from_nanos(20_000_000));
        let mut ctx = ActionContext {
            clock: &clock,
            layout_id: TEST_LAYOUT_ID,
            hardware: &hardware,
            config: &cfg,
            anchor_bias: &anchor_bias,
            scripted_start_pose: None,
            command_seq: 3,
            solver: None,
            single_joint_config: None,
            feedback: None,
        };
        let out = instance.tick(&mut ctx).expect("tick ok");
        match out {
            ActionTickOutput::Completed(ActionOutcome::Success) => {}
            other => panic!("expected Completed(Success), got {other:?}"),
        }
        assert_eq!(instance.successful_hold_target(), Some(vec![1.0, 1.0, 1.0]));
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
            fresh_instance_with_payload(payload, &anchor_bias, start_ns);

        // 先 tick 一帧（验状态确实在跑）。
        let mut ctx = ActionContext {
            clock: &clock,
            layout_id: TEST_LAYOUT_ID,
            hardware: &hardware,
            config: &cfg,
            anchor_bias: &anchor_bias,
            command_seq: 1,
            scripted_start_pose: None,
            solver: None,
            single_joint_config: None,
            feedback: None,
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

    // ====================================================================
    // v0.5 候选 E 段(5)：scripted_start_pose 改源 + 末点发布保证（AC-E17）
    // ====================================================================

    /// tick 期 ctx（起点只在 start() 期读，tick ctx 恒 None）。
    fn tick_ctx<'a>(
        clock: &'a MockClock,
        hardware: &'a HardwareSection,
        cfg: &'a CoordinatorConfig,
        anchor_bias: &'a [f64],
        command_seq: u64,
    ) -> ActionContext<'a> {
        ActionContext {
            clock,
            layout_id: TEST_LAYOUT_ID,
            hardware,
            config: cfg,
            anchor_bias,
            scripted_start_pose: None,
            command_seq,
            solver: None,
            single_joint_config: None,
            feedback: None,
        }
    }

    fn decode_target(body: &JointCommandBody) -> Vec<f64> {
        let cmd = root::<FbJointCommand<'_>>(body.as_bytes()).expect("fbs verify");
        cmd.target().expect("target").iter().collect()
    }

    // ----- 12. start_rejects_missing_scripted_start_pose -----

    #[test]
    fn start_rejects_missing_scripted_start_pose() {
        // D3 第 1 条：起点缺失 → Rejected（→ wire FailedPrecondition），
        // **不** fallback 到 ctx.anchor_bias（anchor 明明合法长度也不用）。
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
            payload_schema_id: SCHEMA_SCRIPTED_PAYLOAD,
            payload,
            idempotency_key: "test-no-start-pose".into(),
            source: ActionSource::Internal,
        };
        let ctx = ActionContext {
            clock: &clock,
            layout_id: TEST_LAYOUT_ID,
            hardware: &hardware,
            config: &cfg,
            anchor_bias: &anchor_bias,
            scripted_start_pose: None,
            command_seq: 0,
            solver: None,
            single_joint_config: None,
            feedback: None,
        };
        let Err(err) = backend.start(&req, &ctx) else {
            panic!("expected Err");
        };
        assert!(
            matches!(err, ActionStartError::Rejected(ref s)
                if s.contains("scripted_start_pose") && s.contains("no fallback")),
            "got {err:?}"
        );
    }

    // ----- 13. start_rejects_scripted_start_pose_length_mismatch -----

    #[test]
    fn start_rejects_scripted_start_pose_length_mismatch() {
        let backend = ScriptedBackend;
        let cfg = CoordinatorConfig::default();
        let hardware = sample_hardware(); // 3 joints
        let anchor_bias = vec![0.0_f64; 3];
        let start_pose = vec![0.0_f64; 2]; // 错长
        let clock = MockClock::new(MonoRawNs(1_000_000_000));
        let payload = build_scripted_payload_bytes(&[(vec![0.1, 0.2, 0.3], 100, 500)]);
        let req = ActionRequest {
            action_id: ActionId::new_v7(),
            kind: ActionKind::Scripted,
            deadline_ns: u64::MAX,
            priority: 10,
            payload_schema_id: SCHEMA_SCRIPTED_PAYLOAD,
            payload,
            idempotency_key: "test-start-pose-len".into(),
            source: ActionSource::Internal,
        };
        let ctx = ActionContext {
            clock: &clock,
            layout_id: TEST_LAYOUT_ID,
            hardware: &hardware,
            config: &cfg,
            anchor_bias: &anchor_bias,
            scripted_start_pose: Some(&start_pose),
            command_seq: 0,
            solver: None,
            single_joint_config: None,
            feedback: None,
        };
        let Err(err) = backend.start(&req, &ctx) else {
            panic!("expected Err");
        };
        assert!(
            matches!(err, ActionStartError::Rejected(ref s)
                if s.contains("scripted_start_pose length 2")),
            "got {err:?}"
        );
    }

    // ----- 14. segment0 from == scripted_start_pose（≠ anchor_bias）-----

    #[test]
    fn segment0_from_is_scripted_start_pose_not_anchor_bias() {
        // start_pose = [2,4,6]，anchor = 哨兵 99；α=0 帧 target 必 == start_pose，
        // α=0.5 帧 == (start_pose + wp)/2。
        let payload = build_scripted_payload_bytes(&[(vec![4.0, 8.0, 12.0], 0, 1000)]);
        let start_pose = vec![2.0_f64, 4.0, 6.0];
        let start_ns: u64 = 1_000_000_000;
        let (mut instance, cfg, hardware, anchor_bias, clock) =
            fresh_instance_with_payload(payload, &start_pose, start_ns);
        assert!(
            anchor_bias
                .iter()
                .all(|v| (*v - ANCHOR_SENTINEL).abs() < 1e-12)
        );

        let mut ctx = tick_ctx(&clock, &hardware, &cfg, &anchor_bias, 1);
        let ActionTickOutput::Continue {
            command: Some(body),
            ..
        } = instance.tick(&mut ctx).expect("tick ok")
        else {
            panic!("alpha=0: expected Continue Some(body)");
        };
        assert_eq!(
            decode_target(&body),
            start_pose,
            "α=0 target == scripted_start_pose"
        );

        clock.advance(robot_core::time::DurationNs::from_nanos(500_000_000));
        let mut ctx = tick_ctx(&clock, &hardware, &cfg, &anchor_bias, 2);
        let ActionTickOutput::Continue {
            command: Some(body),
            ..
        } = instance.tick(&mut ctx).expect("tick ok")
        else {
            panic!("alpha=0.5: expected Continue Some(body)");
        };
        let target = decode_target(&body);
        for (i, (got, want)) in target.iter().zip([3.0, 6.0, 9.0]).enumerate() {
            assert!(
                (got - want).abs() < 1e-9,
                "α=0.5 target[{i}] = {got}, want {want}"
            );
        }
    }

    // ----- 15. AC-E17：dwell=0 末 waypoint 先 Continue(末位) 后 Completed -----

    #[test]
    fn final_waypoint_dwell_zero_emits_continue_then_completed() {
        // 2 waypoint，dwell 均 0，seg 100ms；50ms 步进：
        // t=0 α0 | t=50 α.5 | t=100 wp0 完成→advance→Continue(wp0, 新段起点)
        // | t=150 α.5 | t=200 wp1 完成→advance→**Continue(wp1) 末帧** |
        // t=250 Completed(Success)。
        let wp0 = vec![1.0_f64, 1.0, 1.0];
        let wp1 = vec![2.0_f64, 2.0, 2.0];
        let payload = build_scripted_payload_bytes(&[(wp0.clone(), 0, 100), (wp1.clone(), 0, 100)]);
        let start_ns: u64 = 1_000_000_000;
        let (mut instance, cfg, hardware, anchor_bias, clock) =
            fresh_instance_with_payload(payload, &[0.0; 3], start_ns);

        let mut frames: Vec<(u64, ActionTickOutput)> = Vec::new();
        for step in 0_u64..6 {
            if step > 0 {
                clock.advance(robot_core::time::DurationNs::from_nanos(50_000_000));
            }
            let mut ctx = tick_ctx(&clock, &hardware, &cfg, &anchor_bias, 100 + step);
            let out = instance.tick(&mut ctx).expect("tick ok");
            frames.push((step, out));
        }

        // 前 5 帧全 Continue Some；第 6 帧 Completed(Success)。
        for (step, out) in &frames[..5] {
            assert!(
                matches!(
                    out,
                    ActionTickOutput::Continue {
                        command: Some(_),
                        ..
                    }
                ),
                "step {step}: expected Continue Some, got {out:?}"
            );
        }
        assert!(
            matches!(
                frames[5].1,
                ActionTickOutput::Completed(ActionOutcome::Success)
            ),
            "step 5: expected Completed(Success), got {:?}",
            frames[5].1
        );
        // 第 5 帧（t=200）= 末帧：target == wp1，progress == 1.0。
        let ActionTickOutput::Continue {
            progress,
            command: Some(body),
        } = &frames[4].1
        else {
            unreachable!()
        };
        assert_eq!(decode_target(body), wp1, "末帧 target == 末 waypoint");
        assert!((progress - 1.0).abs() < f32::EPSILON);
        // 第 3 帧（t=100）= wp0 段界帧：target == wp0（既有行为不变）。
        let ActionTickOutput::Continue {
            command: Some(body),
            ..
        } = &frames[2].1
        else {
            unreachable!()
        };
        assert_eq!(
            decode_target(body),
            wp0,
            "段界帧 target == 刚完成的 waypoint"
        );
        assert_eq!(instance.successful_hold_target(), Some(wp1));
    }

    // ----- 16. dwell>0 正常节奏：首个 dwell 帧即置 final_published，dwell
    //          完成后直接 Completed（无额外帧，行为不变）-----

    #[test]
    fn final_waypoint_dwell_positive_normal_cadence_completes_without_extra_frame() {
        // 单 waypoint，seg 100ms + dwell 100ms；50ms 步进：
        // t=0 α0 | t=50 α.5 | t=100 Interp 完→Dwell 过渡帧(target=wp) ←
        // final_published 置位 | t=150 Dwell 帧 | t=200 Dwell 完→advance→
        // final_published 已 true → **直接 Completed**（不多发一帧）。
        let wp = vec![7.0_f64, 7.0, 7.0];
        let payload = build_scripted_payload_bytes(&[(wp.clone(), 100, 100)]);
        let start_ns: u64 = 1_000_000_000;
        let (mut instance, cfg, hardware, anchor_bias, clock) =
            fresh_instance_with_payload(payload, &[0.0; 3], start_ns);

        let mut outs: Vec<ActionTickOutput> = Vec::new();
        let mut hold_after: Vec<Option<Vec<f64>>> = Vec::new();
        for step in 0_u64..5 {
            if step > 0 {
                clock.advance(robot_core::time::DurationNs::from_nanos(50_000_000));
            }
            let mut ctx = tick_ctx(&clock, &hardware, &cfg, &anchor_bias, 10 + step);
            outs.push(instance.tick(&mut ctx).expect("tick ok"));
            hold_after.push(instance.successful_hold_target());
        }
        // t=0 / t=50：Interp 帧，末 target 未发布。
        assert!(hold_after[0].is_none() && hold_after[1].is_none());
        // t=100：Interp→Dwell 过渡帧 target == wp → 已发布。
        let ActionTickOutput::Continue {
            command: Some(body),
            ..
        } = &outs[2]
        else {
            panic!("t=100 expected Continue Some, got {:?}", outs[2]);
        };
        assert_eq!(decode_target(body), wp);
        assert_eq!(
            hold_after[2],
            Some(wp.clone()),
            "首个 dwell 帧即置 final_published"
        );
        // t=150：Dwell 帧。
        assert!(matches!(
            outs[3],
            ActionTickOutput::Continue {
                command: Some(_),
                ..
            }
        ));
        // t=200：Completed(Success)，**无**额外末帧。
        assert!(
            matches!(outs[4], ActionTickOutput::Completed(ActionOutcome::Success)),
            "dwell>0 正常节奏 dwell 完成即 Completed，got {:?}",
            outs[4]
        );
        assert_eq!(hold_after[4], Some(wp));
    }

    // ----- 17. 多 waypoint dwell=0，大步推 clock：仍必须先末帧后 Completed -----

    #[test]
    fn multi_waypoint_big_step_still_emits_final_frame_before_completed() {
        let wp_last = vec![3.0_f64, 3.0, 3.0];
        let payload = build_scripted_payload_bytes(&[
            (vec![1.0, 1.0, 1.0], 0, 100),
            (vec![2.0, 2.0, 2.0], 0, 100),
            (wp_last.clone(), 0, 100),
        ]);
        let start_ns: u64 = 1_000_000_000;
        let (mut instance, cfg, hardware, anchor_bias, clock) =
            fresh_instance_with_payload(payload, &[0.0; 3], start_ns);

        let mut ctx = tick_ctx(&clock, &hardware, &cfg, &anchor_bias, 1);
        let _ = instance.tick(&mut ctx).expect("tick ok");
        // 每 tick 推 10s（远超段时长）：每 tick advance 一段（段界帧 target
        // = 刚完成 waypoint；新段 Interp 起点 = 本 tick now），末段完成的
        // 那一 tick 直接产末帧 → 下一 tick Completed。Completed 之前最后一帧
        // target 必须 == 末 waypoint（既有"advance 只推进一段"语义不变：
        // 段界帧 ×2 + 末帧 + Completed = 4 tick，i = 0..=3）。
        let mut last_target: Option<Vec<f64>> = None;
        let mut completed_at: Option<u64> = None;
        for i in 0_u64..10 {
            clock.advance(robot_core::time::DurationNs::from_nanos(10_000_000_000));
            let mut ctx = tick_ctx(&clock, &hardware, &cfg, &anchor_bias, 10 + i);
            match instance.tick(&mut ctx).expect("tick ok") {
                ActionTickOutput::Continue {
                    command: Some(body),
                    ..
                } => last_target = Some(decode_target(&body)),
                ActionTickOutput::Continue { command: None, .. } => {
                    panic!("Scripted 永不返 Continue(None)")
                }
                ActionTickOutput::Completed(ActionOutcome::Success) => {
                    completed_at = Some(i);
                    break;
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(
            completed_at,
            Some(3),
            "段界帧 ×2 + 末帧后第 4 tick（i=3）Completed"
        );
        assert_eq!(
            last_target.as_deref(),
            Some(wp_last.as_slice()),
            "Completed 之前最后一帧 target 必须 == 末 waypoint"
        );
        assert_eq!(instance.successful_hold_target(), Some(wp_last));
    }

    // ----- 18. cancel 于末帧之后：backend 仍返 PartialSuccess（coordinator
    //          走 Cancelled → reset；successful_hold_target 返值不参与）-----

    #[test]
    fn cancel_after_final_frame_still_converges_partial_success() {
        let payload = build_scripted_payload_bytes(&[(vec![1.0, 1.0, 1.0], 0, 100)]);
        let start_ns: u64 = 1_000_000_000;
        let (mut instance, cfg, hardware, anchor_bias, clock) =
            fresh_instance_with_payload(payload, &[0.0; 3], start_ns);
        let mut ctx = tick_ctx(&clock, &hardware, &cfg, &anchor_bias, 1);
        let _ = instance.tick(&mut ctx).expect("tick ok");
        clock.advance(robot_core::time::DurationNs::from_nanos(500_000_000));
        let mut ctx = tick_ctx(&clock, &hardware, &cfg, &anchor_bias, 2);
        let out = instance.tick(&mut ctx).expect("tick ok");
        assert!(matches!(
            out,
            ActionTickOutput::Continue {
                command: Some(_),
                ..
            }
        ));
        assert!(instance.successful_hold_target().is_some());

        instance.cancel(&mut ctx).expect("cancel ok");
        let out = instance.tick(&mut ctx).expect("tick ok");
        match out {
            ActionTickOutput::Completed(ActionOutcome::PartialSuccess { warning }) => {
                assert_eq!(warning, "cancelled_at_waypoint_1");
            }
            other => panic!("expected Completed(PartialSuccess), got {other:?}"),
        }
    }
}

#[cfg(test)]
mod single_joint_tests {
    use super::*;
    use crate::action::{ActionId, ActionSource};
    use crate::coordinator::{CoordinatorConfig, HardwareSection};
    use crate::single_joint::{JointMotionLimits, SingleJointConfig};
    use crate::state_cache::JointFeedbackSnapshot;
    use flatbuffers::FlatBufferBuilder;
    use robot_core::time::{MockClock, MonoRawNs, RobotClock};
    use robot_kinematics_xlerobot::Side;
    use robot_msg::msg::{
        JointCommand, SingleJointPositionPayload, SingleJointPositionPayloadArgs,
    };

    const BASE_NS: u64 = 1_000_000_000;
    // 模拟 S0 的真实 f32 位置反馈提升到 wire f64 后的值。
    const START: [f64; 3] = [(-0.2_f32) as f64, 0.1_f32 as f64, 0.3_f32 as f64];
    const TARGET: [f64; 3] = [START[0], 0.5, START[2]];

    struct Fixture {
        hardware: HardwareSection,
        config: CoordinatorConfig,
        limits: SingleJointConfig,
        clock: MockClock,
    }

    impl Fixture {
        fn new() -> Self {
            let hardware = HardwareSection {
                id: "定位总线".into(),
                joints: vec!["关节甲".into(), "关节乙".into(), "关节丙".into()],
                motor_ids: vec![1, 2, 3],
                feetech_bus2_mixed_raw: false,
                calibration_range_tolerances_raw: None,
            };
            let limits = SingleJointConfig {
                max_duration_ms: 2_000,
                settle_timeout_ms: 500,
                stable_window_ms: 100,
                joints: hardware
                    .joints
                    .iter()
                    .map(|name| JointMotionLimits {
                        name: name.clone(),
                        min_position_rad: -1.0,
                        max_position_rad: 1.0,
                        max_velocity_rad_s: 3.0,
                        tolerance_rad: 0.001,
                    })
                    .collect(),
            };
            Self {
                hardware,
                config: CoordinatorConfig::default(),
                limits,
                clock: MockClock::new(MonoRawNs(BASE_NS)),
            }
        }

        fn mixed() -> Self {
            let mut fixture = Self::new();
            fixture.hardware = crate::coordinator::parse_hardware_section_from_toml(
                include_str!("../../../../configs/xlerobot-bus2.toml"),
                "right_arm_full_with_base",
            )
            .unwrap();
            fixture.hardware.feetech_bus2_mixed_raw = true;
            fixture.hardware.calibration_range_tolerances_raw = None;
            fixture.hardware.joints.rotate_left(4);
            fixture.hardware.motor_ids.rotate_left(4);
            fixture.limits.joints = fixture
                .hardware
                .joints
                .iter()
                .enumerate()
                .filter(|(slot, _)| !fixture.hardware.is_wheel_slot(*slot))
                .map(|(_, name)| JointMotionLimits {
                    name: name.clone(),
                    ..fixture.limits.joints[0].clone()
                })
                .collect();
            fixture.limits.joints.reverse();
            fixture
        }

        fn context<'a>(&'a self, feedback: Option<&'a JointFeedbackSnapshot>) -> ActionContext<'a> {
            ActionContext {
                clock: &self.clock,
                layout_id: 7,
                hardware: &self.hardware,
                config: &self.config,
                anchor_bias: &[99.0; 3],
                scripted_start_pose: Some(&START),
                command_seq: 42,
                solver: None,
                single_joint_config: Some(&self.limits),
                feedback,
            }
        }

        fn request(
            &self,
            hardware: &str,
            joint: &str,
            target: f64,
            duration_ms: u32,
        ) -> ActionRequest {
            let mut builder = FlatBufferBuilder::new();
            let hardware_id = builder.create_string(hardware);
            let joint_name = builder.create_string(joint);
            let payload = SingleJointPositionPayload::create(
                &mut builder,
                &SingleJointPositionPayloadArgs {
                    hardware_id: Some(hardware_id),
                    joint_name: Some(joint_name),
                    target_position_rad: target,
                    duration_ms,
                },
            );
            builder.finish(payload, None);
            ActionRequest {
                action_id: ActionId::new_v7(),
                kind: ActionKind::Scripted,
                deadline_ns: self.clock.now().0 + 2_000_000_000,
                priority: 10,
                payload_schema_id: SCHEMA_SINGLE_JOINT_POSITION_PAYLOAD,
                payload: builder.finished_data().to_vec(),
                idempotency_key: "单关节离线测试".into(),
                source: ActionSource::Internal,
            }
        }

        fn start(&self) -> Box<dyn ActionInstance> {
            self.start_from(&self.request("定位总线", "关节乙", 0.5, 200), &START)
                .unwrap()
        }

        fn start_from(
            &self,
            request: &ActionRequest,
            start: &[f64],
        ) -> Result<Box<dyn ActionInstance>, ActionStartError> {
            let feedback = sample(0, start);
            let mut context = self.context(Some(&feedback));
            context.scripted_start_pose = Some(start);
            ScriptedBackend.start(request, &context)
        }

        fn tick(
            &self,
            instance: &mut dyn ActionInstance,
            milliseconds: u64,
            feedback: Option<&JointFeedbackSnapshot>,
            publish: bool,
        ) -> Result<ActionTickOutput, ActionFailure> {
            self.clock.set(MonoRawNs(at(milliseconds)));
            let mut context = self.context(feedback);
            context.scripted_start_pose = None;
            let output = instance.tick(&mut context)?;
            if publish
                && matches!(
                    output,
                    ActionTickOutput::Continue {
                        command: Some(_),
                        ..
                    }
                )
            {
                instance.command_published(self.clock.now().0);
            }
            Ok(output)
        }

        fn ready_tick(
            &self,
            instance: &mut dyn ActionInstance,
            milliseconds: u64,
            positions: &[f64],
        ) -> ActionTickOutput {
            self.tick(
                instance,
                milliseconds,
                Some(&sample(milliseconds, positions)),
                true,
            )
            .unwrap()
        }
    }

    const fn at(milliseconds: u64) -> u64 {
        BASE_NS + milliseconds * 1_000_000
    }

    fn sample(milliseconds: u64, positions: &[f64]) -> JointFeedbackSnapshot {
        JointFeedbackSnapshot {
            stamp_ns: at(milliseconds),
            positions: positions.to_vec(),
            motion_ready: true,
        }
    }

    fn command(output: ActionTickOutput) -> Vec<f64> {
        let ActionTickOutput::Continue {
            command: Some(body),
            ..
        } = output
        else {
            panic!("应继续输出位置命令");
        };
        let command = flatbuffers::root::<JointCommand<'_>>(body.as_bytes()).unwrap();
        assert_eq!(command.source_task_id(), 42);
        command.target().unwrap().iter().collect()
    }

    #[test]
    fn single_joint_interpolates_only_selected_target_and_preserves_published_hold() {
        let mut fixture = Fixture::new();
        fixture.limits.joints.reverse();
        let mut instance = fixture.start();
        assert_eq!(instance.interrupted_hold_target(), Some(START.to_vec()));
        assert_eq!(
            command(fixture.ready_tick(instance.as_mut(), 0, &START)),
            START
        );
        let half = command(fixture.ready_tick(instance.as_mut(), 100, &START));
        assert_eq!(half[0], START[0]);
        assert_eq!(half[1], f64::from(0.3_f32));
        assert_eq!(half[2], START[2]);
        assert_eq!(instance.interrupted_hold_target(), Some(half));
        assert_eq!(
            command(fixture.ready_tick(instance.as_mut(), 200, &START)),
            TARGET
        );
        assert_eq!(instance.interrupted_hold_target(), Some(TARGET.to_vec()));
    }

    #[test]
    fn single_joint_rejects_f32_target_rounding_outside_exact_limits() {
        for target in [0.1, -0.1] {
            let mut fixture = Fixture::new();
            if target > 0.0 {
                fixture.limits.joints[1].max_position_rad = target;
            } else {
                fixture.limits.joints[1].min_position_rad = target;
            }
            let start = [START[0], 0.0, START[2]];
            let request = fixture.request("定位总线", "关节乙", target, 200);
            assert!(
                matches!(fixture.start_from(&request, &start), Err(ActionStartError::Rejected(reason)) if reason.contains("f32 编码值"))
            );
            // 相邻的内侧 f32 值可接受；不以夹紧目标代替拒绝外侧编码值。
            let inside = f64::from(f32::from_bits(0.1_f32.to_bits() - 1)) * target.signum();
            assert!(
                fixture
                    .start_from(&fixture.request("定位总线", "关节乙", inside, 200), &start)
                    .is_ok()
            );
        }
    }

    #[test]
    fn single_joint_checks_both_requested_and_encoded_velocity_without_tolerance() {
        let mut fixture = Fixture::new();
        let start = [START[0], 0.0, START[2]];
        let request = fixture.request("定位总线", "关节乙", 0.1, 1_000);
        fixture.limits.joints[1].max_velocity_rad_s = 0.1;
        assert!(
            matches!(fixture.start_from(&request, &start), Err(ActionStartError::Rejected(reason)) if reason.contains("速率"))
        );
        fixture.limits.joints[1].max_velocity_rad_s = f64::from(0.1_f32);
        assert!(fixture.start_from(&request, &start).is_ok());
        // 向内舍入也不能使原请求超速变为可接受。
        fixture.limits.joints[1].max_velocity_rad_s = f64::from(0.7_f32);
        let request = fixture.request("定位总线", "关节乙", 0.7, 1_000);
        assert!(
            matches!(fixture.start_from(&request, &start), Err(ActionStartError::Rejected(reason)) if reason.contains("速率"))
        );
    }

    #[test]
    fn single_joint_rejects_encoded_start_or_unselected_hold_outside_limits() {
        let mut fixture = Fixture::new();
        fixture.limits.joints[0].max_position_rad = 0.1;
        let request = fixture.request("定位总线", "关节乙", 0.5, 200);
        for first_position in [0.1, f64::from(0.1_f32)] {
            let start = [first_position, 0.0, START[2]];
            assert!(
                matches!(fixture.start_from(&request, &start), Err(ActionStartError::Rejected(reason)) if reason.contains("反馈位置"))
            );
        }
        // 物理反馈已是 f32 时，在其实际值边界可用且未选关节保持完全相同。
        fixture.limits.joints[0].max_position_rad = f64::from(0.1_f32);
        let start = [f64::from(0.1_f32), 0.0, START[2]];
        let mut instance = fixture.start_from(&request, &start).unwrap();
        assert_eq!(
            command(fixture.ready_tick(instance.as_mut(), 0, &start)),
            start
        );
        let final_target = command(fixture.ready_tick(instance.as_mut(), 200, &start));
        assert_eq!(final_target[0], start[0]);
        assert_eq!(final_target[2], start[2]);
    }

    #[test]
    fn single_joint_uses_encoded_target_for_trajectory_hold_and_zero_tolerance_confirmation() {
        let mut fixture = Fixture::new();
        for joint in &mut fixture.limits.joints {
            joint.tolerance_rad = 0.0;
        }
        let start = [START[0], 0.0, START[2]];
        let target = [START[0], f64::from(0.1_f32), START[2]];
        let request = fixture.request("定位总线", "关节乙", 0.1, 200);
        let mut instance = fixture.start_from(&request, &start).unwrap();
        assert_eq!(
            command(fixture.ready_tick(instance.as_mut(), 0, &start)),
            start
        );
        let half = command(fixture.ready_tick(instance.as_mut(), 100, &start));
        assert_eq!(half, [start[0], f64::from(0.05_f32), start[2]]);
        assert_eq!(instance.interrupted_hold_target(), Some(half));
        assert_eq!(
            command(fixture.ready_tick(instance.as_mut(), 200, &start)),
            target
        );
        assert_eq!(instance.interrupted_hold_target(), Some(target.to_vec()));
        command(fixture.ready_tick(instance.as_mut(), 210, &target));
        assert!(matches!(
            fixture.ready_tick(instance.as_mut(), 310, &target),
            ActionTickOutput::Completed(ActionOutcome::Success)
        ));
        assert_eq!(instance.successful_hold_target(), Some(target.to_vec()));
    }

    #[test]
    fn single_joint_rejects_identity_target_duration_and_rate_errors() {
        let fixture = Fixture::new();
        let feedback = sample(0, &START);
        let context = fixture.context(Some(&feedback));
        for (hardware, joint, target, duration) in [
            ("其他总线", "关节乙", 0.5, 200),
            ("定位总线", "未知关节", 0.5, 200),
            ("定位总线", "关节乙", f64::NAN, 200),
            ("定位总线", "关节乙", f64::INFINITY, 200),
            ("定位总线", "关节乙", 1.1, 200),
            ("定位总线", "关节乙", 0.5, 0),
            ("定位总线", "关节乙", 0.5, 2_001),
            ("定位总线", "关节乙", 0.5, 10),
        ] {
            assert!(matches!(
                ScriptedBackend.start(
                    &fixture.request(hardware, joint, target, duration),
                    &context
                ),
                Err(ActionStartError::Rejected(_))
            ));
        }
    }

    #[test]
    fn single_joint_rejects_missing_config_invalid_mixed_layout_and_mismatched_snapshot() {
        let mut fixture = Fixture::new();
        let feedback = sample(0, &START);
        let request = fixture.request("定位总线", "关节乙", 0.5, 200);
        let mut context = fixture.context(Some(&feedback));
        context.single_joint_config = None;
        assert!(ScriptedBackend.start(&request, &context).is_err());
        let mut context = fixture.context(Some(&feedback));
        context.scripted_start_pose = Some(&TARGET);
        assert!(ScriptedBackend.start(&request, &context).is_err());
        assert!(
            ScriptedBackend
                .start(&request, &fixture.context(None))
                .is_err()
        );
        fixture.hardware.feetech_bus2_mixed_raw = true;
        assert!(
            ScriptedBackend
                .start(&request, &fixture.context(Some(&feedback)))
                .is_err()
        );
    }

    #[test]
    fn single_joint_rejects_bad_start_feedback_and_insufficient_deadline() {
        let fixture = Fixture::new();
        let request = fixture.request("定位总线", "关节乙", 0.5, 200);
        for feedback in [
            sample(0, &[1.1, 0.1, 0.3]),
            sample(0, &[f64::NAN, 0.1, 0.3]),
            sample(0, &[0.1]),
        ] {
            let mut context = fixture.context(Some(&feedback));
            context.scripted_start_pose = Some(&feedback.positions);
            assert!(ScriptedBackend.start(&request, &context).is_err());
        }
        let feedback = sample(0, &START);
        let mut too_short = request.clone();
        too_short.deadline_ns = at(700) - 1;
        assert!(
            ScriptedBackend
                .start(&too_short, &fixture.context(Some(&feedback)))
                .is_err()
        );
        fixture.clock.set(MonoRawNs(u64::MAX - 10));
        let mut overflow = request;
        overflow.deadline_ns = u64::MAX;
        let feedback = JointFeedbackSnapshot {
            stamp_ns: u64::MAX - 10,
            positions: START.to_vec(),
            motion_ready: true,
        };
        assert!(
            ScriptedBackend
                .start(&overflow, &fixture.context(Some(&feedback)))
                .is_err()
        );
    }

    #[test]
    fn single_joint_completion_requires_new_feedback_after_actual_final_publish() {
        let fixture = Fixture::new();
        let mut instance = fixture.start();
        fixture.ready_tick(instance.as_mut(), 0, &START);
        let output = fixture
            .tick(instance.as_mut(), 200, Some(&sample(200, &TARGET)), false)
            .unwrap();
        assert_eq!(command(output), TARGET);
        assert!(instance.successful_hold_target().is_none());
        instance.command_published(at(205));
        // 这帧晚于命令生成，仍早于实际发布成功，不得开始稳定窗口。
        command(
            fixture
                .tick(instance.as_mut(), 210, Some(&sample(203, &TARGET)), true)
                .unwrap(),
        );
        command(fixture.ready_tick(instance.as_mut(), 250, &TARGET));
        // 相同缓存帧被反复读取，不能按本地时间累计到位。
        command(
            fixture
                .tick(instance.as_mut(), 350, Some(&sample(250, &TARGET)), true)
                .unwrap(),
        );
        assert!(matches!(
            fixture.ready_tick(instance.as_mut(), 351, &TARGET),
            ActionTickOutput::Completed(ActionOutcome::Success)
        ));
        assert_eq!(instance.successful_hold_target(), Some(TARGET.to_vec()));
    }

    #[test]
    fn single_joint_unpublished_final_command_cannot_complete() {
        let fixture = Fixture::new();
        let mut instance = fixture.start();
        fixture.ready_tick(instance.as_mut(), 0, &START);
        for milliseconds in [200, 300, 400] {
            command(
                fixture
                    .tick(
                        instance.as_mut(),
                        milliseconds,
                        Some(&sample(milliseconds, &TARGET)),
                        false,
                    )
                    .unwrap(),
            );
        }
        assert!(instance.successful_hold_target().is_none());
        assert_eq!(instance.interrupted_hold_target(), Some(START.to_vec()));
    }

    #[test]
    fn single_joint_checks_all_joints_and_resets_broken_stable_window() {
        let fixture = Fixture::new();
        let mut instance = fixture.start();
        fixture.ready_tick(instance.as_mut(), 0, &START);
        fixture.ready_tick(instance.as_mut(), 200, &START);
        command(fixture.ready_tick(instance.as_mut(), 210, &TARGET));
        command(fixture.ready_tick(instance.as_mut(), 260, &[-0.19, 0.5, 0.3]));
        command(fixture.ready_tick(instance.as_mut(), 310, &TARGET));
        command(fixture.ready_tick(instance.as_mut(), 409, &TARGET));
        assert!(matches!(
            fixture.ready_tick(instance.as_mut(), 410, &TARGET),
            ActionTickOutput::Completed(ActionOutcome::Success)
        ));
    }

    #[test]
    fn single_joint_settle_timeout_does_not_report_success_or_refresh_on_hold() {
        let fixture = Fixture::new();
        let mut instance = fixture.start();
        fixture.ready_tick(instance.as_mut(), 0, &START);
        fixture.ready_tick(instance.as_mut(), 200, &START);
        command(fixture.ready_tick(instance.as_mut(), 699, &START));
        assert!(matches!(
            fixture.tick(instance.as_mut(), 700, Some(&sample(700, &START)), true),
            Err(ActionFailure::PreconditionFailed(_))
        ));
        assert_eq!(instance.interrupted_hold_target(), Some(TARGET.to_vec()));
    }

    #[test]
    fn single_joint_feedback_failure_preserves_last_published_target() {
        for bad_feedback in [
            None,
            Some(JointFeedbackSnapshot {
                motion_ready: false,
                ..sample(110, &START)
            }),
            Some(sample(111, &START)),
            Some(sample(99, &START)),
            Some(sample(110, &[0.0])),
            Some(sample(110, &[f64::NAN, 0.1, 0.3])),
            Some(sample(110, &[1.1, 0.1, 0.3])),
        ] {
            let fixture = Fixture::new();
            let mut instance = fixture.start();
            fixture.ready_tick(instance.as_mut(), 0, &START);
            let last = command(fixture.ready_tick(instance.as_mut(), 100, &START));
            assert!(matches!(
                fixture.tick(instance.as_mut(), 110, bad_feedback.as_ref(), true),
                Err(ActionFailure::SafetyAbort(_))
            ));
            assert_eq!(instance.interrupted_hold_target(), Some(last));
        }
        let fixture = Fixture::new();
        let mut instance = fixture.start();
        fixture.ready_tick(instance.as_mut(), 0, &START);
        assert!(matches!(
            fixture.tick(instance.as_mut(), 201, Some(&sample(0, &START)), true),
            Err(ActionFailure::SafetyAbort(_))
        ));
    }

    #[test]
    fn single_joint_cancel_without_feedback_never_commits_unsent_target() {
        let fixture = Fixture::new();
        let mut instance = fixture.start();
        fixture.ready_tick(instance.as_mut(), 0, &START);
        command(
            fixture
                .tick(instance.as_mut(), 100, Some(&sample(100, &START)), false)
                .unwrap(),
        );
        instance.cancel(&mut fixture.context(None)).unwrap();
        assert!(matches!(
            fixture.tick(instance.as_mut(), 110, None, true).unwrap(),
            ActionTickOutput::Completed(ActionOutcome::PartialSuccess { .. })
        ));
        assert_eq!(instance.interrupted_hold_target(), Some(START.to_vec()));
    }

    #[test]
    fn single_joint_cancel_before_first_tick_uses_submission_snapshot() {
        let fixture = Fixture::new();
        let mut instance = fixture.start();
        instance.cancel(&mut fixture.context(None)).unwrap();
        assert!(matches!(
            fixture.tick(instance.as_mut(), 10, None, true).unwrap(),
            ActionTickOutput::Completed(ActionOutcome::PartialSuccess { .. })
        ));
        assert_eq!(instance.interrupted_hold_target(), Some(START.to_vec()));
    }

    #[test]
    fn single_joint_deadline_precedes_cancel_and_feedback_checks() {
        let fixture = Fixture::new();
        let mut instance = fixture.start();
        instance.cancel(&mut fixture.context(None)).unwrap();
        assert!(matches!(
            fixture.tick(instance.as_mut(), 2_000, None, true),
            Err(ActionFailure::DeadlineExceeded)
        ));
        assert_eq!(instance.interrupted_hold_target(), Some(START.to_vec()));
    }

    fn mixed_feedback(fixture: &Fixture) -> Vec<f64> {
        fixture
            .hardware
            .motor_ids
            .iter()
            .map(|&id| {
                if id <= 6 {
                    f64::from(id) / 16.0
                } else {
                    4000.0
                }
            })
            .collect()
    }

    fn mixed_command(output: ActionTickOutput, hardware: &HardwareSection) -> Vec<f64> {
        let ActionTickOutput::Continue {
            command: Some(body),
            ..
        } = output
        else {
            panic!("应继续输出完整混合命令");
        };
        let command = flatbuffers::root::<JointCommand<'_>>(body.as_bytes()).unwrap();
        assert_eq!(
            command.mode(),
            robot_msg::msg::ControlMode::MixedPositionRawVelocity
        );
        assert_eq!(command.on_timeout(), robot_msg::msg::StaleAction::Hold);
        assert_eq!(
            command.raw_velocities().unwrap().iter().collect::<Vec<_>>(),
            [0; 9]
        );
        let targets: Vec<_> = command.target().unwrap().iter().collect();
        assert_eq!(targets.len(), hardware.joints.len());
        for (slot, target) in targets.iter().enumerate() {
            if hardware.is_wheel_slot(slot) {
                assert_eq!(target.to_bits(), 0.0_f64.to_bits());
            }
        }
        targets
    }

    fn home_request(fixture: &Fixture, hardware: &str, side: Side) -> ActionRequest {
        use robot_msg::msg::{
            LeftArmHomePayload, LeftArmHomePayloadArgs, RightArmHomePayload,
            RightArmHomePayloadArgs,
        };

        let mut builder = FlatBufferBuilder::new();
        let hardware_id = Some(builder.create_string(hardware));
        let payload_schema_id = match side {
            Side::Left => {
                let root = LeftArmHomePayload::create(
                    &mut builder,
                    &LeftArmHomePayloadArgs { hardware_id },
                );
                builder.finish(root, None);
                SCHEMA_LEFT_ARM_HOME_PAYLOAD
            }
            Side::Right => {
                let root = RightArmHomePayload::create(
                    &mut builder,
                    &RightArmHomePayloadArgs { hardware_id },
                );
                builder.finish(root, None);
                SCHEMA_RIGHT_ARM_HOME_PAYLOAD
            }
        };
        ActionRequest {
            action_id: ActionId::new_v7(),
            kind: ActionKind::Scripted,
            deadline_ns: fixture.clock.now().0 + 40_000_000_000,
            priority: 10,
            payload_schema_id,
            payload: builder.finished_data().to_vec(),
            idempotency_key: "单臂回位模拟".into(),
            source: ActionSource::Internal,
        }
    }

    #[test]
    fn right_arm_home_moves_five_named_axes_keeps_gripper_and_wheels_and_confirms_arrival() {
        let mut fixture = Fixture::mixed();
        fixture.limits = crate::coordinator::parse_single_joint_config_from_toml(
            include_str!("../../../../configs/xlerobot-bus2-motion.toml"),
            &fixture.hardware,
        )
        .unwrap()
        .unwrap();
        let mut feedback = mixed_feedback(&fixture);
        let wrist = fixture
            .hardware
            .motor_ids
            .iter()
            .position(|&id| id == 5)
            .unwrap();
        feedback[wrist] = -2.5;
        let request = home_request(&fixture, &fixture.hardware.id, Side::Right);
        let mut instance = fixture.start_from(&request, &feedback).unwrap();
        let mut source = feedback.clone();
        fixture.hardware.clear_wheel_targets(&mut source);
        let mut target = source.clone();
        for (slot, &id) in fixture.hardware.motor_ids.iter().enumerate() {
            if id <= 5 {
                target[slot] = 0.0;
            }
        }
        for (t, expected) in [
            (0, source.clone()),
            (
                12_500,
                source
                    .iter()
                    .zip(&target)
                    .map(|(a, b)| f64::midpoint(*a, *b))
                    .collect(),
            ),
            (25_000, target.clone()),
        ] {
            let actual = mixed_command(
                fixture.ready_tick(instance.as_mut(), t, &feedback),
                &fixture.hardware,
            );
            assert_eq!(actual, expected);
        }
        // 到位由末帧之后的新鲜反馈确认，沿用原配置 ±20 raw 和稳定窗。
        feedback.clone_from(&target);
        feedback[wrist] = f64::from(robot_adapter_feetech::raw_to_rad(2047 + 20));
        mixed_command(
            fixture.ready_tick(instance.as_mut(), 25_020, &feedback),
            &fixture.hardware,
        );
        assert!(matches!(
            fixture.ready_tick(
                instance.as_mut(),
                25_020 + u64::from(fixture.limits.stable_window_ms),
                &feedback
            ),
            ActionTickOutput::Completed(ActionOutcome::Success)
        ));
        assert_eq!(instance.successful_hold_target(), Some(target));
    }

    #[test]
    fn right_arm_home_cancel_holds_last_published_positions_and_zero_wheels() {
        let fixture = Fixture::mixed();
        let feedback = mixed_feedback(&fixture);
        let request = home_request(&fixture, &fixture.hardware.id, Side::Right);
        let mut instance = fixture.start_from(&request, &feedback).unwrap();
        mixed_command(
            fixture.ready_tick(instance.as_mut(), 0, &feedback),
            &fixture.hardware,
        );
        let midway = mixed_command(
            fixture.ready_tick(instance.as_mut(), 5_000, &feedback),
            &fixture.hardware,
        );
        instance.cancel(&mut fixture.context(None)).unwrap();
        assert!(matches!(
            fixture.tick(instance.as_mut(), 5_010, None, false).unwrap(),
            ActionTickOutput::Completed(ActionOutcome::PartialSuccess { .. })
        ));
        assert_eq!(instance.interrupted_hold_target(), Some(midway));
    }

    #[test]
    fn right_arm_home_rejects_wrong_identity_layout_deadline_and_feedback() {
        let mut fixture = Fixture::mixed();
        let feedback = mixed_feedback(&fixture);
        let request = home_request(&fixture, &fixture.hardware.id, Side::Right);
        assert!(
            fixture
                .start_from(&home_request(&fixture, "其他总线", Side::Right), &feedback)
                .is_err()
        );
        assert!(fixture.start_from(&request, &feedback[..3]).is_err());
        let mut invalid = feedback.clone();
        invalid[0] = f64::NAN;
        assert!(fixture.start_from(&request, &invalid).is_err());
        let mut short = home_request(&fixture, &fixture.hardware.id, Side::Right);
        short.deadline_ns = fixture.clock.now().0 + 10_000_000_000;
        assert!(fixture.start_from(&short, &feedback).is_err());
        let mut bad_payload = home_request(&fixture, &fixture.hardware.id, Side::Right);
        bad_payload.payload.clear();
        assert!(matches!(
            fixture.start_from(&bad_payload, &feedback),
            Err(ActionStartError::PayloadDecode(_))
        ));
        let wrist = fixture
            .hardware
            .joints
            .iter()
            .position(|name| name == "right_arm_wrist_roll")
            .unwrap();
        fixture.hardware.joints[wrist] = "left_arm_wrist_roll".into();
        assert!(fixture.start_from(&request, &feedback).is_err());
        let left = Fixture::new();
        assert!(
            left.start_from(&home_request(&left, &left.hardware.id, Side::Right), &START)
                .is_err()
        );
    }

    fn left_home_fixture() -> Fixture {
        let mut fixture = Fixture::new();
        fixture.hardware = crate::coordinator::parse_hardware_section_from_toml(
            include_str!("../../../../configs/xlerobot-bus1.toml"),
            "left_arm_full_with_head",
        )
        .unwrap();
        fixture.limits = crate::coordinator::parse_single_joint_config_from_toml(
            include_str!("../../../../configs/xlerobot-bus1-motion.toml"),
            &fixture.hardware,
        )
        .unwrap()
        .unwrap();
        fixture
    }

    #[test]
    fn left_arm_home_holds_gripper_and_both_head_axes_until_arrival_or_cancel() {
        let fixture = left_home_fixture();
        let start = [0.25, -0.5, 1.0, -0.75, 2.5, 0.375, -0.25, 0.5];
        let goal = [0.0, 0.0, 0.0, 0.0, 0.0, start[5], start[6], start[7]];
        let request = home_request(&fixture, &fixture.hardware.id, Side::Left);
        let mut instance = fixture.start_from(&request, &start).unwrap();
        assert_eq!(
            command(fixture.ready_tick(instance.as_mut(), 0, &start)),
            start
        );
        let middle = command(fixture.ready_tick(instance.as_mut(), 12_500, &start));
        assert_eq!(
            middle,
            std::array::from_fn::<_, 8, _>(|i| f64::midpoint(start[i], goal[i]))
        );
        assert_eq!(
            command(fixture.ready_tick(instance.as_mut(), 25_000, &middle)),
            goal
        );
        let mut settled = goal;
        settled[2] = f64::from(robot_adapter_feetech::raw_to_rad(2047 - 20));
        assert_eq!(
            command(fixture.ready_tick(instance.as_mut(), 25_020, &settled)),
            goal
        );
        assert!(matches!(
            fixture.ready_tick(instance.as_mut(), 25_220, &settled),
            ActionTickOutput::Completed(ActionOutcome::Success)
        ));
        assert_eq!(instance.successful_hold_target(), Some(goal.to_vec()));

        fixture.clock.set(MonoRawNs(BASE_NS));
        let mut instance = fixture.start_from(&request, &start).unwrap();
        command(fixture.ready_tick(instance.as_mut(), 0, &start));
        assert_eq!(
            command(fixture.ready_tick(instance.as_mut(), 12_500, &start)),
            middle
        );
        instance.cancel(&mut fixture.context(None)).unwrap();
        assert!(matches!(
            fixture
                .tick(instance.as_mut(), 12_520, None, false)
                .unwrap(),
            ActionTickOutput::Completed(ActionOutcome::PartialSuccess { .. })
        ));
        assert_eq!(instance.interrupted_hold_target(), Some(middle));
    }

    #[test]
    fn left_arm_home_rejects_cross_bus_and_incomplete_head_layouts() {
        let mut left = left_home_fixture();
        let right = Fixture::mixed();
        for (fixture, side) in [(&left, Side::Right), (&right, Side::Left)] {
            let start = vec![0.0; fixture.hardware.joints.len()];
            assert!(
                fixture
                    .start_from(&home_request(fixture, &fixture.hardware.id, side), &start)
                    .is_err()
            );
        }
        let request = home_request(&left, &left.hardware.id, Side::Left);
        left.hardware.joints[7] = "head_motor_1".into();
        assert!(left.start_from(&request, &[0.0; 8]).is_err());
        left.hardware.joints[7] = "head_motor_2".into();
        left.hardware.motor_ids.swap(4, 7);
        assert!(left.start_from(&request, &[0.0; 8]).is_err());
    }

    #[test]
    fn arm_home_slots_follow_paired_config_order_but_reject_wrong_physical_ids() {
        use robot_kinematics_xlerobot::arm_home_slots;
        for (mut fixture, side) in [
            (left_home_fixture(), Side::Left),
            (Fixture::mixed(), Side::Right),
        ] {
            let hardware = &mut fixture.hardware;
            hardware.joints.rotate_left(2);
            hardware.motor_ids.rotate_left(2);
            let slots = |hw: &HardwareSection| {
                arm_home_slots(
                    side,
                    &hw.id,
                    &hw.joints,
                    &hw.motor_ids,
                    hw.feetech_bus2_mixed_raw,
                )
            };
            assert_eq!(
                slots(hardware)
                    .unwrap()
                    .map(|slot| hardware.motor_ids[slot]),
                [1, 2, 3, 4, 5]
            );
            let ids = |id| {
                hardware
                    .motor_ids
                    .iter()
                    .position(|&value| value == id)
                    .unwrap()
            };
            let (wrist, grip, other1, other2) = (ids(5), ids(6), ids(7), ids(8));
            hardware.motor_ids.swap(wrist, grip);
            assert!(slots(hardware).is_err());
            hardware.motor_ids.swap(wrist, grip);
            hardware.motor_ids.swap(other1, other2);
            assert_eq!(slots(hardware).is_ok(), side == Side::Right);
        }
    }

    #[test]
    fn mixed_single_joint_rejects_wheel_requests_even_for_zero_target() {
        let fixture = Fixture::mixed();
        let feedback = mixed_feedback(&fixture);
        for (slot, name) in fixture.hardware.joints.iter().enumerate() {
            if fixture.hardware.is_wheel_slot(slot) {
                for target in [0.0, 0.5, 500.0] {
                    let request = fixture.request(&fixture.hardware.id, name, target, 200);
                    assert!(fixture.start_from(&request, &feedback).is_err());
                }
            }
        }
    }

    #[test]
    fn mixed_single_joint_keeps_real_elbow_boundary_anchor_and_strict_active_target() {
        let mut fixture = Fixture::new();
        fixture.hardware = crate::coordinator::parse_hardware_section_from_toml(
            include_str!("../../../../configs/xlerobot-bus2.toml"),
            "right_arm_full_with_base",
        )
        .unwrap();
        fixture.limits = crate::coordinator::parse_single_joint_config_from_toml(
            include_str!("../../../../configs/xlerobot-bus2-motion.toml"),
            &fixture.hardware,
        )
        .unwrap()
        .unwrap();
        let elbow = fixture
            .hardware
            .motor_ids
            .iter()
            .position(|&id| id == 3)
            .unwrap();
        let wrist = fixture
            .hardware
            .motor_ids
            .iter()
            .position(|&id| id == 5)
            .unwrap();
        let rad = |raw| f64::from(robot_adapter_feetech::raw_to_rad(raw));
        let request = fixture.request(
            &fixture.hardware.id,
            &fixture.hardware.joints[wrist],
            rad(2071),
            1000,
        );
        for raw in [792, 2998, 3011] {
            fixture.clock.set(MonoRawNs(BASE_NS));
            let mut feedback = vec![0.0; 9];
            feedback[elbow] = rad(raw);
            let mut instance = fixture.start_from(&request, &feedback).unwrap();
            for time in [0, 500, 1000] {
                let target = mixed_command(
                    fixture.ready_tick(instance.as_mut(), time, &feedback),
                    &fixture.hardware,
                );
                assert_eq!(target[elbow].to_bits(), feedback[elbow].to_bits());
                for slot in 0..6 {
                    if slot != wrist {
                        assert_eq!(target[slot], feedback[slot]);
                    }
                }
            }
        }
        fixture.clock.set(MonoRawNs(BASE_NS));
        for raw in [791, 3012] {
            let mut feedback = vec![0.0; 9];
            feedback[elbow] = rad(raw);
            assert!(fixture.start_from(&request, &feedback).is_err());
        }
        let mut feedback = vec![0.0; 9];
        feedback[elbow] = rad(2998);
        for raw in [811, 2992, 3011] {
            let target_request = fixture.request(
                &fixture.hardware.id,
                &fixture.hardware.joints[elbow],
                rad(raw),
                1000,
            );
            assert!(fixture.start_from(&target_request, &feedback).is_err());
        }
        let mut instance = fixture.start_from(&request, &feedback).unwrap();
        feedback[elbow] = rad(3012);
        assert!(
            fixture
                .tick(instance.as_mut(), 10, Some(&sample(10, &feedback)), false)
                .is_err()
        );
    }

    #[test]
    fn mixed_single_joint_keeps_wheels_zero_and_confirms_only_position_feedback() {
        let fixture = Fixture::mixed();
        let mut feedback = mixed_feedback(&fixture);
        let selected = fixture
            .hardware
            .motor_ids
            .iter()
            .position(|&id| id == 5)
            .unwrap();
        let request = fixture.request(
            &fixture.hardware.id,
            &fixture.hardware.joints[selected],
            0.75,
            200,
        );
        let mut instance = fixture.start_from(&request, &feedback).unwrap();
        let mut initial = feedback.clone();
        fixture.hardware.clear_wheel_targets(&mut initial);
        assert_eq!(instance.interrupted_hold_target(), Some(initial.clone()));
        assert_eq!(
            mixed_command(
                fixture.ready_tick(instance.as_mut(), 0, &feedback),
                &fixture.hardware
            ),
            initial
        );
        let halfway = mixed_command(
            fixture.ready_tick(instance.as_mut(), 100, &feedback),
            &fixture.hardware,
        );
        assert_eq!(halfway[selected], f64::midpoint(initial[selected], 0.75));
        for (slot, &target) in halfway.iter().enumerate() {
            if slot != selected {
                assert_eq!(target, initial[slot]);
            }
        }
        let mut final_target = initial;
        final_target[selected] = 0.75;
        assert_eq!(
            mixed_command(
                fixture.ready_tick(instance.as_mut(), 200, &feedback),
                &fixture.hardware
            ),
            final_target
        );
        feedback[selected] = 0.75;
        for (slot, position) in feedback.iter_mut().enumerate() {
            if fixture.hardware.is_wheel_slot(slot) {
                *position = -4096.0;
            }
        }
        mixed_command(
            fixture.ready_tick(instance.as_mut(), 220, &feedback),
            &fixture.hardware,
        );
        assert!(matches!(
            fixture.ready_tick(instance.as_mut(), 320, &feedback),
            ActionTickOutput::Completed(ActionOutcome::Success)
        ));
        assert_eq!(instance.successful_hold_target(), Some(final_target));
    }

    #[test]
    fn mixed_single_joint_cancel_and_bad_feedback_keep_last_published_zero_wheels() {
        for cancel in [true, false] {
            let fixture = Fixture::mixed();
            let feedback = mixed_feedback(&fixture);
            let selected = fixture
                .hardware
                .motor_ids
                .iter()
                .position(|&id| id == 5)
                .unwrap();
            let request = fixture.request(
                &fixture.hardware.id,
                &fixture.hardware.joints[selected],
                0.75,
                200,
            );
            let mut instance = fixture.start_from(&request, &feedback).unwrap();
            mixed_command(
                fixture.ready_tick(instance.as_mut(), 0, &feedback),
                &fixture.hardware,
            );
            let committed = mixed_command(
                fixture.ready_tick(instance.as_mut(), 100, &feedback),
                &fixture.hardware,
            );
            mixed_command(
                fixture
                    .tick(instance.as_mut(), 150, Some(&sample(150, &feedback)), false)
                    .unwrap(),
                &fixture.hardware,
            );
            if cancel {
                instance.cancel(&mut fixture.context(None)).unwrap();
                assert!(matches!(
                    fixture.tick(instance.as_mut(), 160, None, false).unwrap(),
                    ActionTickOutput::Completed(ActionOutcome::PartialSuccess { .. })
                ));
            } else {
                let mut invalid = feedback;
                let wheel = fixture
                    .hardware
                    .motor_ids
                    .iter()
                    .position(|&id| id == 7)
                    .unwrap();
                invalid[wheel] = f64::NAN;
                assert!(matches!(
                    fixture.tick(instance.as_mut(), 160, Some(&sample(160, &invalid)), false),
                    Err(ActionFailure::SafetyAbort(_))
                ));
            }
            assert_eq!(instance.interrupted_hold_target(), Some(committed));
        }
    }
}
