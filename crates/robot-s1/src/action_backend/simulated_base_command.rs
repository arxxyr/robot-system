//! 模拟专用的有界三轮命令，沿原 InvokeTask、Coordinator 和单 publisher。
//!
//! 仅 `test-base-command` 特性和显式 `simulated_base_command` profile 可接入。
//! 使用既有混合 JointCommand 载荷，位置字段作为提交时原位前提，三轮各仅
//! 允许 raw0/+500。租期从提交时固定，后续 tick 或心跳不续期；断联仅由该
//! 短租期兜底，不把 RPC 连接关闭伪装成主动取消。零速发布不是硬件停止读回。

use crate::action::{
    ActionBackend, ActionCancelError, ActionContext, ActionFailure, ActionInstance, ActionKind,
    ActionOutcome, ActionRequest, ActionStartError, ActionTickOutput, JointCommandBody,
};
use crate::single_joint::{SingleJointConfig, position_for_s0};
use robot_adapter_feetech::velocity_profile::Sts3215RawWheelProfile;
use robot_core::wheel_velocity_profile::COMMAND_TTL_MAX_NS;
use robot_msg::build_helpers::build_mixed_joint_command;
use robot_msg::msg::{ControlMode, JointCommand, StaleAction};
use robot_msg::schema_id::SCHEMA_JOINT_COMMAND;

/// 原混合命令输入序号只在成功提交时消费；输出仍使用原 writer 序号。
pub(crate) fn request_sequence(req: &ActionRequest) -> Result<u64, ActionStartError> {
    let command = flatbuffers::root::<JointCommand<'_>>(&req.payload)
        .map_err(|error| ActionStartError::PayloadDecode(error.to_string()))?;
    if command.source_task_id() == 0 {
        return Err(ActionStartError::Rejected(
            "模拟底盘输入序号必须非零".into(),
        ));
    }
    Ok(command.source_task_id())
}

#[derive(Debug, Clone, Copy)]
pub struct SimulatedBaseCommandBackend;

#[allow(clippy::unnecessary_literal_bound)]
impl ActionBackend for SimulatedBaseCommandBackend {
    fn name(&self) -> &str {
        "simulated_base_command"
    }

    fn supported_kinds(&self) -> &[ActionKind] {
        &[ActionKind::Scripted]
    }

    fn supported_schemas(&self) -> &[u32] {
        &[SCHEMA_JOINT_COMMAND]
    }

    fn start(
        &self,
        req: &ActionRequest,
        ctx: &ActionContext<'_>,
    ) -> Result<Box<dyn ActionInstance>, ActionStartError> {
        let reject = |reason: &str| ActionStartError::Rejected(reason.into());
        if req.payload_schema_id != SCHEMA_JOINT_COMMAND
            || !ctx.hardware.feetech_bus2_mixed_raw
            || !ctx.hardware.mixed_layout_is_valid()
        {
            return Err(reject(
                "模拟底盘请求需要既有完整右侧混合布局与JointCommand载荷",
            ));
        }
        let command = flatbuffers::root::<JointCommand<'_>>(&req.payload)
            .map_err(|error| ActionStartError::PayloadDecode(error.to_string()))?;
        if command.layout_id() != ctx.layout_id
            || command.mode() != ControlMode::MixedPositionRawVelocity
            || command.on_timeout() != StaleAction::Hold
        {
            return Err(reject("模拟底盘布局、混合模式或超时合同不符"));
        }
        let now = ctx.clock.now().0;
        let deadline = command.deadline_ns().min(req.deadline_ns);
        if !deadline
            .checked_sub(now)
            .is_some_and(|ttl| (1..=COMMAND_TTL_MAX_NS).contains(&ttl))
        {
            return Err(reject("模拟底盘运动租期必须大于0且不超过100毫秒"));
        }
        let config = ctx
            .single_joint_config
            .ok_or_else(|| reject("缺少右臂原位保持约束"))?;
        config
            .validate(ctx.hardware)
            .map_err(ActionStartError::Rejected)?;
        let feedback = ctx
            .feedback
            .ok_or_else(|| reject("模拟底盘缺少新鲜完整反馈"))?;
        let positions = ctx
            .scripted_start_pose
            .ok_or_else(|| reject("模拟底盘缺少原位快照"))?;
        if feedback.positions != positions || !feedback.motion_ready {
            return Err(reject("模拟底盘需要同一快照及使能、活动、无故障状态"));
        }
        config
            .validate_hold_positions(ctx.hardware, positions)
            .map_err(ActionStartError::Rejected)?;
        let (Some(target), Some(raw)) = (command.target(), command.raw_velocities()) else {
            return Err(reject("模拟底盘缺少完整九槽目标"));
        };
        if target.len() != positions.len() || raw.len() != positions.len() {
            return Err(reject("模拟底盘目标维度不符"));
        }
        let mut held: Vec<f64> = positions.iter().copied().map(position_for_s0).collect();
        let mut velocities = [0_i16; 9];
        for (slot, position) in held.iter_mut().enumerate() {
            if ctx.hardware.is_wheel_slot(slot) {
                if target.get(slot) != 0.0 || !Sts3215RawWheelProfile::accepts_raw(raw.get(slot)) {
                    return Err(reject("三轮仅接受原混合合同raw0/+500，轮位置必须为0"));
                }
                velocities[slot] = raw.get(slot);
                *position = 0.0;
            } else if raw.get(slot) != 0
                || !target.get(slot).is_finite()
                || position_for_s0(target.get(slot)).partial_cmp(position)
                    != Some(std::cmp::Ordering::Equal)
            {
                return Err(reject("底盘请求不得改变右臂原位或填写臂速度"));
            }
        }
        let instance = BaseCommandInstance {
            held,
            velocities,
            config: config.clone(),
            deadline,
            last_tick: now,
            last_feedback: feedback.stamp_ns,
            cancelled: false,
            terminal: None,
            zero_published: false,
            nonzero_published: false,
        };
        instance
            .check_feedback(ctx, now)
            .map_err(|error| reject(&error.to_string()))?;
        Ok(Box::new(instance))
    }
}

struct BaseCommandInstance {
    held: Vec<f64>,
    velocities: [i16; 9],
    config: SingleJointConfig,
    deadline: u64,
    last_tick: u64,
    last_feedback: u64,
    cancelled: bool,
    terminal: Option<Result<ActionOutcome, ActionFailure>>,
    zero_published: bool,
    // 只记录原publisher成功回调，不把生成命令或实际轮子运动混为一谈。
    nonzero_published: bool,
}

impl BaseCommandInstance {
    fn check_feedback(&self, ctx: &ActionContext<'_>, now: u64) -> Result<u64, ActionFailure> {
        let feedback = ctx
            .feedback
            .ok_or_else(|| ActionFailure::SafetyAbort("底盘缺少新鲜反馈".into()))?;
        if !feedback.motion_ready
            || feedback.stamp_ns == 0
            || feedback.stamp_ns > now
            || now - feedback.stamp_ns > ctx.config.max_stale_ms.saturating_mul(1_000_000)
            || feedback.stamp_ns < self.last_feedback
        {
            return Err(ActionFailure::SafetyAbort(
                "底盘反馈未活动、陈旧或时间回退".into(),
            ));
        }
        self.config
            .validate_hold_positions(ctx.hardware, &feedback.positions)
            .map_err(ActionFailure::SafetyAbort)?;
        for (slot, &position) in feedback.positions.iter().enumerate() {
            if !ctx.hardware.is_wheel_slot(slot) {
                let limit = self
                    .config
                    .joints
                    .iter()
                    .find(|limit| limit.name == ctx.hardware.joints[slot])
                    .ok_or_else(|| ActionFailure::SafetyAbort("右臂保持约束缺失".into()))?;
                if (position - self.held[slot]).abs() > limit.tolerance_rad {
                    return Err(ActionFailure::SafetyAbort(
                        "底盘动作期间右臂偏离原位".into(),
                    ));
                }
            }
        }
        Ok(feedback.stamp_ns)
    }
}

impl ActionInstance for BaseCommandInstance {
    fn tick(&mut self, ctx: &mut ActionContext<'_>) -> Result<ActionTickOutput, ActionFailure> {
        let now = ctx.clock.now().0;
        // 安全失败优先于取消和正常到期；待零速确认期间的新故障也不能被掩盖。
        if !matches!(self.terminal, Some(Err(_))) {
            let safety = if now < self.last_tick {
                Err(ActionFailure::SafetyAbort("底盘命令时钟回退".into()))
            } else {
                self.check_feedback(ctx, now)
            };
            match safety {
                Ok(stamp) => self.last_feedback = stamp,
                Err(error) => self.terminal = Some(Err(error)),
            }
        }
        if self.zero_published
            && let Some(result) = self.terminal.as_ref()
        {
            return result.clone().map(ActionTickOutput::Completed);
        }
        if self.terminal.is_none() {
            self.terminal = if self.cancelled {
                Some(Ok(ActionOutcome::PartialSuccess {
                    warning: "主动取消底盘命令".into(),
                }))
            } else if self.velocities.iter().all(|&raw| raw == 0) {
                Some(Ok(ActionOutcome::Success))
            } else {
                // 提前一个原命令周期归零，避免正常到期仍发送新的非零帧。
                // 这只缩短固定租期；S0原超时故障与失联保护不变。
                let period =
                    u64::try_from(ctx.config.command_period().as_nanos()).unwrap_or(u64::MAX);
                if now.saturating_add(period) >= self.deadline {
                    let warning = if self.nonzero_published {
                        "底盘固定租期到达归零边界，未续期"
                    } else {
                        "底盘固定租期到达归零边界，未发布非零命令"
                    };
                    Some(Ok(ActionOutcome::PartialSuccess {
                        warning: warning.into(),
                    }))
                } else {
                    None
                }
            };
        }
        self.last_tick = now;
        let zero = self.terminal.is_some();
        let raw = if zero { &[0; 9] } else { &self.velocities };
        let deadline = now.saturating_add(ctx.config.deadline_offset_ns());
        let body = build_mixed_joint_command(
            ctx.layout_id,
            &self.held,
            raw,
            if zero {
                deadline
            } else {
                deadline.min(self.deadline)
            },
            ctx.config.priority,
            ctx.command_seq,
        );
        Ok(ActionTickOutput::Continue {
            progress: if zero { 1.0 } else { 0.0 },
            command: Some(JointCommandBody::new(body)),
        })
    }

    fn command_published(&mut self, _published_at_ns: u64) {
        if self.terminal.is_some() {
            self.zero_published = true;
        } else {
            self.nonzero_published = true;
        }
    }

    fn cancel(&mut self, _ctx: &mut ActionContext<'_>) -> Result<(), ActionCancelError> {
        self.cancelled = true;
        Ok(())
    }

    fn interrupted_hold_target(&self) -> Option<Vec<f64>> {
        Some(self.held.clone())
    }

    fn successful_hold_target(&self) -> Option<Vec<f64>> {
        Some(self.held.clone())
    }
}

static BACKEND: SimulatedBaseCommandBackend = SimulatedBaseCommandBackend;
inventory::submit! { &BACKEND as &'static dyn ActionBackend }
