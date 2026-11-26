//! 单关节与右臂 IK 共用的位置约束、插值计划和反馈到位判据。
//!
//! 目标只替换提交时快照中选中的位置轴，其他位置轴保持。
//! 混合总线仅配置位置关节，三轮目标恒为 raw0，轮反馈不参与位置到位判断。
//! 中断保持使用最后成功发布的目标，不能解释为当前位置或已断扭。

use serde::Deserialize;

#[cfg(feature = "backend-scripted")]
use crate::action::{ActionContext, ActionFailure, ActionRequest, ActionStartError};
use crate::coordinator::HardwareSection;
#[cfg(feature = "backend-scripted")]
use crate::state_cache::JointFeedbackSnapshot;
#[cfg(feature = "backend-scripted")]
use robot_msg::msg::{LeftArmHomePayload, RightArmHomePayload, SingleJointPositionPayload};

/// 单关节定位参数；数值须由当前硬件校准和已验证的运动限制明确给出。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SingleJointConfig {
    pub max_duration_ms: u32,
    pub settle_timeout_ms: u32,
    pub stable_window_ms: u32,
    pub joints: Vec<JointMotionLimits>,
}

/// 按关节名称绑定的位置范围、运动速率与实测到位容差。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JointMotionLimits {
    pub name: String,
    pub min_position_rad: f64,
    pub max_position_rad: f64,
    pub max_velocity_rad_s: f64,
    pub tolerance_rad: f64,
}

impl SingleJointConfig {
    /// 启动及断扭后的原位保持使用硬件边界容差，绝不裁剪快照。
    pub fn validate_hold_positions(
        &self,
        hardware: &HardwareSection,
        positions: &[f64],
    ) -> Result<(), String> {
        if positions.len() != hardware.joints.len() || !hardware.range_tolerances_valid() {
            return Err("原位快照或校准容差维度无效".into());
        }
        for (slot, &position) in positions.iter().enumerate() {
            if !position.is_finite() || position.abs() > f64::from(f32::MAX) {
                return Err("原位位置含非有限或不可表示值".into());
            }
            if hardware.is_wheel_slot(slot) {
                continue;
            }
            let limit = self
                .joints
                .iter()
                .find(|limit| limit.name == hardware.joints[slot])
                .ok_or_else(|| format!("缺少原位约束：{}", hardware.joints[slot]))?;
            if !valid_hold_position(position, limit, hardware.range_tolerance_raw(slot)) {
                return Err(format!("原位位置超出校准保持范围：{}", limit.name));
            }
        }
        Ok(())
    }

    /// 校验完整位置关节覆盖与参数边界；不为轮槽补造 rad 运动参数。
    pub fn validate(&self, hardware: &HardwareSection) -> Result<(), String> {
        if !hardware.mixed_layout_is_valid() {
            return Err("混合总线需要完整右臂九槽及 ID1..9".into());
        }
        if !hardware.range_tolerances_valid() {
            return Err("校准边界容差必须与关节等长且在0..4095内".into());
        }
        if self.max_duration_ms == 0
            || self.settle_timeout_ms == 0
            || self.stable_window_ms == 0
            || self.stable_window_ms >= self.settle_timeout_ms
        {
            return Err("定位时长、到位超时及稳定窗口须非零，稳定窗口必须小于到位超时".into());
        }
        if hardware.joints.is_empty() {
            return Err("单关节定位硬件不得缺少关节".into());
        }
        for (index, name) in hardware.joints.iter().enumerate() {
            if hardware.joints[..index].contains(name) {
                return Err(format!("硬件关节名称重复：{name}"));
            }
            if !hardware.is_wheel_slot(index)
                && !self.joints.iter().any(|joint| joint.name == *name)
            {
                return Err(format!("单关节运动约束缺少关节：{name}"));
            }
        }
        for (index, joint) in self.joints.iter().enumerate() {
            let Some(slot) = hardware.joints.iter().position(|name| *name == joint.name) else {
                return Err(format!("单关节运动约束包含未知关节：{}", joint.name));
            };
            if hardware.is_wheel_slot(slot) {
                return Err(format!("轮关节不得配置 rad 位置运动约束：{}", joint.name));
            }
            if self.joints[..index]
                .iter()
                .any(|previous| previous.name == joint.name)
            {
                return Err(format!("单关节运动约束重复：{}", joint.name));
            }
            if !joint.min_position_rad.is_finite()
                || !joint.max_position_rad.is_finite()
                || joint.min_position_rad >= joint.max_position_rad
                || !joint.max_velocity_rad_s.is_finite()
                || joint.max_velocity_rad_s <= 0.0
                || !joint.tolerance_rad.is_finite()
                || joint.tolerance_rad < 0.0
            {
                return Err(format!("关节 {} 的运动边界、速率或容差无效", joint.name));
            }
            if hold_range(joint, hardware.range_tolerance_raw(slot)).is_none() {
                return Err(format!(
                    "关节 {} 的原位边界必须来自可往返的校准raw端点",
                    joint.name
                ));
            }
        }
        Ok(())
    }
}

/// 已通过提交校验的一条完整位置目标；由既有 Scripted 插值器执行。
#[cfg(feature = "backend-scripted")]
pub(crate) struct SingleJointPlan {
    pub(crate) initial_pose: Vec<f64>,
    pub(crate) positions: Vec<f64>,
    pub(crate) duration_ms: u32,
    pub(crate) execution: SingleJointExecution,
}

/// 单关节请求专有的确认状态；旧全向量 Scripted 不使用此状态。
#[cfg(feature = "backend-scripted")]
pub(crate) struct SingleJointExecution {
    deadline_ns: u64,
    settle_timeout_ns: u64,
    stable_window_ns: u64,
    max_stale_ns: u64,
    /// 配置槽位顺序的保持/反馈范围；到位容差保持原值，None 只表示轮槽。
    limits: Vec<Option<JointMotionLimits>>,
    target: Vec<f64>,
    last_feedback_stamp_ns: u64,
    final_published_at_ns: Option<u64>,
    stable_since_ns: Option<u64>,
    last_published_target: Vec<f64>,
}

#[cfg(feature = "backend-scripted")]
impl SingleJointPlan {
    pub(crate) fn prepare(
        req: &ActionRequest,
        ctx: &ActionContext<'_>,
    ) -> Result<Self, ActionStartError> {
        let config = ctx.single_joint_config.ok_or_else(|| {
            ActionStartError::Rejected("未配置 s1.single_joint，拒绝单关节定位".into())
        })?;
        let payload =
            flatbuffers::root::<SingleJointPositionPayload<'_>>(&req.payload).map_err(|error| {
                ActionStartError::PayloadDecode(format!("单关节请求解码失败：{error}"))
            })?;
        if payload.hardware_id() != ctx.hardware.id.as_str() {
            return Err(ActionStartError::Rejected(
                "单关节请求的硬件身份不匹配".into(),
            ));
        }
        let selected = ctx
            .hardware
            .joints
            .iter()
            .position(|name| payload.joint_name() == name.as_str())
            .ok_or_else(|| ActionStartError::Rejected("单关节请求包含未知关节名称".into()))?;
        if payload.duration_ms() > config.max_duration_ms {
            return Err(ActionStartError::Rejected(
                "单关节运动时长超出显式配置范围".into(),
            ));
        }
        Self::prepare_positions(
            req,
            ctx,
            config,
            payload.duration_ms(),
            &[(selected, payload.target_position_rad())],
        )
    }

    pub(crate) fn arm_home(
        req: &ActionRequest,
        ctx: &ActionContext<'_>,
    ) -> Result<Self, ActionStartError> {
        use robot_kinematics_xlerobot::{Side, arm_home_duration_ms, arm_home_slots};
        use robot_msg::schema_id::{SCHEMA_LEFT_ARM_HOME_PAYLOAD, SCHEMA_RIGHT_ARM_HOME_PAYLOAD};

        let hardware = ctx.hardware;
        let decode_error =
            |error| ActionStartError::PayloadDecode(format!("手臂回位请求无效：{error}"));
        let (side, requested_hardware) = match req.payload_schema_id {
            SCHEMA_LEFT_ARM_HOME_PAYLOAD => (
                Side::Left,
                flatbuffers::root::<LeftArmHomePayload<'_>>(&req.payload)
                    .map_err(decode_error)?
                    .hardware_id(),
            ),
            SCHEMA_RIGHT_ARM_HOME_PAYLOAD => (
                Side::Right,
                flatbuffers::root::<RightArmHomePayload<'_>>(&req.payload)
                    .map_err(decode_error)?
                    .hardware_id(),
            ),
            _ => return Err(ActionStartError::Rejected("未知的回位请求".into())),
        };
        if requested_hardware != hardware.id {
            return Err(ActionStartError::Rejected(
                "回位请求的硬件身份不匹配".into(),
            ));
        }
        let slots = arm_home_slots(
            side,
            &hardware.id,
            &hardware.joints,
            &hardware.motor_ids,
            hardware.feetech_bus2_mixed_raw,
        )
        .map_err(|reason| ActionStartError::Rejected(reason.into()))?;
        let config = ctx
            .single_joint_config
            .ok_or_else(|| ActionStartError::Rejected("手臂回位缺少运动约束".into()))?;
        let feedback = ctx
            .feedback
            .ok_or_else(|| ActionStartError::Rejected("手臂回位缺少本次反馈".into()))?;
        let mut targets = [(0, 0.0); 5];
        let mut start = [0.0; 5];
        let mut speeds = [0.0; 5];
        for (axis, slot) in slots.into_iter().enumerate() {
            let name = &hardware.joints[slot];
            targets[axis] = (slot, 0.0);
            start[axis] = position_for_s0(*feedback.positions.get(slot).ok_or_else(|| {
                ActionStartError::Rejected("回位反馈维度与硬件布局不匹配".into())
            })?);
            speeds[axis] = config
                .joints
                .iter()
                .find(|joint| joint.name == *name)
                .ok_or_else(|| ActionStartError::Rejected(format!("缺少回位速率：{name}")))?
                .max_velocity_rad_s;
        }
        let duration_ms = arm_home_duration_ms(start, speeds)
            .map_err(|message| ActionStartError::Rejected(message.into()))?;
        Self::prepare_positions(req, ctx, config, duration_ms, &targets)
    }

    /// 用本次反馈保持未选位置轴，所选轴共同沿同一时间轴插值并确认到位。
    pub(crate) fn prepare_positions(
        req: &ActionRequest,
        ctx: &ActionContext<'_>,
        config: &SingleJointConfig,
        duration_ms: u32,
        targets: &[(usize, f64)],
    ) -> Result<Self, ActionStartError> {
        config
            .validate(ctx.hardware)
            .map_err(ActionStartError::Rejected)?;

        if duration_ms == 0 {
            return Err(ActionStartError::Rejected(
                "单关节运动时长超出显式配置范围".into(),
            ));
        }
        let now_ns = ctx.clock.now().0;
        let required_ns = u64::from(duration_ms)
            .checked_add(u64::from(config.settle_timeout_ms))
            .and_then(|milliseconds| milliseconds.checked_mul(1_000_000))
            .and_then(|nanoseconds| now_ns.checked_add(nanoseconds))
            .ok_or_else(|| ActionStartError::Rejected("单关节定位截止时间计算溢出".into()))?;
        if req.deadline_ns < required_ns {
            return Err(ActionStartError::Rejected(
                "请求剩余期限不足运动时长与到位确认预算".into(),
            ));
        }
        let start_pose = ctx
            .scripted_start_pose
            .ok_or_else(|| ActionStartError::Rejected("单关节定位缺少提交时完整位置快照".into()))?;
        let feedback = ctx
            .feedback
            .ok_or_else(|| ActionStartError::Rejected("单关节定位缺少提交时新鲜反馈".into()))?;
        if feedback.positions.as_slice() != start_pose {
            return Err(ActionStartError::Rejected(
                "单关节起点与反馈必须来自同一快照".into(),
            ));
        }
        let limits = limits_in_slot_order(config, ctx.hardware)?;
        // validate 已确认逐关节覆盖；反馈维度仍由执行期共用校验独立守门。
        let mut execution = SingleJointExecution {
            deadline_ns: req.deadline_ns,
            settle_timeout_ns: u64::from(config.settle_timeout_ms) * 1_000_000,
            stable_window_ns: u64::from(config.stable_window_ms) * 1_000_000,
            max_stale_ns: ctx.config.max_stale_ms.saturating_mul(1_000_000),
            limits,
            target: start_pose.to_vec(),
            last_feedback_stamp_ns: 0,
            final_published_at_ns: None,
            stable_since_ns: None,
            last_published_target: start_pose.to_vec(),
        };
        execution
            .observe_feedback(now_ns, Some(feedback))
            .map_err(|error| ActionStartError::Rejected(error.to_string()))?;
        // S0 将位置命令降为 f32；真实 f32 反馈提升后的值在此转换中不变。
        let mut initial_pose: Vec<_> = start_pose.iter().copied().map(position_for_s0).collect();
        ctx.hardware.clear_wheel_targets(&mut initial_pose);
        execution.target.clone_from(&initial_pose);
        execution.last_published_target.clone_from(&initial_pose);
        for &(slot, target) in targets {
            let name = ctx
                .hardware
                .joints
                .get(slot)
                .ok_or_else(|| ActionStartError::Rejected("定位槽位越界".into()))?;
            if ctx.hardware.is_wheel_slot(slot) {
                return Err(ActionStartError::Rejected(
                    "定位只能选择位置关节，三轮目标保持 raw0".into(),
                ));
            }
            let limit = config
                .joints
                .iter()
                .find(|limit| limit.name == *name)
                .ok_or_else(|| ActionStartError::Rejected("所选关节缺少位置约束".into()))?;
            execution.target[slot] = if (req.kind == crate::action::ActionKind::Ik
                && ctx.hardware.feetech_bus2_mixed_raw)
                || matches!(
                    req.payload_schema_id,
                    robot_msg::schema_id::SCHEMA_RIGHT_ARM_HOME_PAYLOAD
                        | robot_msg::schema_id::SCHEMA_LEFT_ARM_HOME_PAYLOAD
                ) {
                let raw = checked_encoder_target(target, initial_pose[slot], duration_ms, limit)?;
                f64::from(robot_adapter_feetech::raw_to_rad(raw))
            } else {
                checked_motion_target(target, initial_pose[slot], duration_ms, limit)?
            };
        }
        Ok(Self {
            initial_pose,
            positions: execution.target.clone(),
            duration_ms,
            execution,
        })
    }
}

/// 按硬件槽位排列位置约束，轮槽保留 None，不给轮子补造 rad 参数。
#[cfg(feature = "backend-scripted")]
fn limits_in_slot_order(
    config: &SingleJointConfig,
    hardware: &HardwareSection,
) -> Result<Vec<Option<JointMotionLimits>>, ActionStartError> {
    hardware
        .joints
        .iter()
        .enumerate()
        .map(|(slot, name)| {
            if hardware.is_wheel_slot(slot) {
                return Ok(None);
            }
            config
                .joints
                .iter()
                .find(|limit| limit.name == *name)
                .ok_or_else(|| ActionStartError::Rejected(format!("缺少关节约束：{name}")))
                .and_then(|limit| {
                    let (min, max) = hold_range(limit, hardware.range_tolerance_raw(slot))
                        .ok_or_else(|| {
                            ActionStartError::Rejected(format!("原位范围无效：{name}"))
                        })?;
                    let mut held = limit.clone();
                    held.min_position_rad = min;
                    held.max_position_rad = max;
                    Ok(Some(held))
                })
        })
        .collect()
}

/// 原请求与实际编码值同时守范围、速率；返回用于执行与到位确认的编码目标。
#[cfg(feature = "backend-scripted")]
fn checked_motion_target(
    requested_target: f64,
    start: f64,
    duration_ms: u32,
    limit: &JointMotionLimits,
) -> Result<f64, ActionStartError> {
    robot_adapter_feetech::convert::checked_motion_position(
        requested_target,
        start,
        duration_ms,
        [limit.min_position_rad, limit.max_position_rad],
        limit.max_velocity_rad_s,
    )
    .map_err(|message| ActionStartError::Rejected(message.into()))
}

/// 右臂 IK 和只读维护计划共用电机实际接收的整数刻度，原浮点解与取整目标均须合法。
#[cfg(feature = "backend-scripted")]
pub fn checked_encoder_target(
    requested: f64,
    start: f64,
    duration_ms: u32,
    limit: &JointMotionLimits,
) -> Result<i32, ActionStartError> {
    robot_adapter_feetech::convert::checked_position_target(
        requested,
        start,
        duration_ms,
        [limit.min_position_rad, limit.max_position_rad],
        limit.max_velocity_rad_s,
    )
    .map_err(|message| ActionStartError::Rejected(message.into()))
}

pub(crate) fn valid_position(position: f64, limit: &JointMotionLimits) -> bool {
    robot_adapter_feetech::convert::within_active_range(
        position,
        [limit.min_position_rad, limit.max_position_rad],
    )
}

fn hold_range(limit: &JointMotionLimits, tolerance_raw: u16) -> Option<(f64, f64)> {
    robot_adapter_feetech::convert::calibrated_hold_range(
        limit.min_position_rad,
        limit.max_position_rad,
        tolerance_raw,
    )
}

pub(crate) fn valid_hold_position(
    position: f64,
    limit: &JointMotionLimits,
    tolerance_raw: u16,
) -> bool {
    if tolerance_raw == 0 {
        return valid_position(position, limit);
    }
    hold_range(limit, tolerance_raw).is_some_and(|(min, max)| {
        position.is_finite()
            && position.abs() <= f64::from(f32::MAX)
            && (min..=max).contains(&position)
            && (min..=max).contains(&position_for_s0(position))
    })
}

/// 沿 S0 位置命令的实际 f32 编码，再提升以供同一轨迹、保持及到位比较使用。
/// 范围校验同时检查转换前后；此转换不裁剪越界值。
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn position_for_s0(position: f64) -> f64 {
    f64::from(position as f32)
}

#[cfg(feature = "backend-scripted")]
impl SingleJointExecution {
    /// 总期限先于合作式取消判定，避免已超时任务被标为正常取消。
    pub(crate) fn check_deadline(&self, now_ns: u64) -> Result<(), ActionFailure> {
        if now_ns >= self.deadline_ns {
            return Err(ActionFailure::DeadlineExceeded);
        }
        Ok(())
    }

    /// 所有位置关节连续满足容差后才返回 true；重复源帧不延长稳定窗口。
    pub(crate) fn observe_feedback(
        &mut self,
        now_ns: u64,
        feedback: Option<&JointFeedbackSnapshot>,
    ) -> Result<bool, ActionFailure> {
        let feedback = feedback
            .ok_or_else(|| ActionFailure::SafetyAbort("单关节定位缺少新鲜完整反馈".into()))?;
        if !feedback.motion_ready {
            return Err(ActionFailure::SafetyAbort(
                "单关节反馈未满足使能、活动、上扭及无故障条件".into(),
            ));
        }
        if feedback.stamp_ns == 0
            || feedback.stamp_ns > now_ns
            || now_ns - feedback.stamp_ns > self.max_stale_ns
            || feedback.stamp_ns < self.last_feedback_stamp_ns
        {
            return Err(ActionFailure::SafetyAbort(
                "单关节反馈时间戳无效、陈旧或回退".into(),
            ));
        }
        if feedback.positions.len() != self.limits.len()
            || feedback
                .positions
                .iter()
                .zip(&self.limits)
                .any(|(&position, limit)| {
                    !position.is_finite()
                        || position.abs() > f64::from(f32::MAX)
                        || limit
                            .as_ref()
                            .is_some_and(|limit| !valid_position(position, limit))
                })
        {
            return Err(ActionFailure::SafetyAbort(
                "单关节反馈位置不完整、非有限或越界".into(),
            ));
        }
        let advanced = feedback.stamp_ns > self.last_feedback_stamp_ns;
        self.last_feedback_stamp_ns = feedback.stamp_ns;
        let Some(final_published_at_ns) = self.final_published_at_ns else {
            return Ok(false);
        };
        if now_ns.saturating_sub(final_published_at_ns) >= self.settle_timeout_ns {
            return Err(ActionFailure::PreconditionFailed(
                "单关节实测到位确认超时".into(),
            ));
        }
        if feedback.stamp_ns <= final_published_at_ns {
            return Ok(false);
        }
        if feedback
            .positions
            .iter()
            .zip(&self.target)
            .zip(&self.limits)
            .any(|((&position, &target), limit)| {
                limit
                    .as_ref()
                    .is_some_and(|limit| (position - target).abs() > limit.tolerance_rad)
            })
        {
            self.stable_since_ns = None;
            return Ok(false);
        }
        if !advanced {
            return Ok(false);
        }
        let stable_since_ns = *self.stable_since_ns.get_or_insert(feedback.stamp_ns);
        Ok(feedback.stamp_ns - stable_since_ns >= self.stable_window_ns)
    }

    /// 仅发布成功后更新；首次末目标成功发布启动到位计时，后续保活不续期。
    pub(crate) fn command_published(
        &mut self,
        target: &[f64],
        final_target: bool,
        published_at_ns: u64,
    ) {
        self.last_published_target.copy_from_slice(target);
        if final_target && self.final_published_at_ns.is_none() {
            self.final_published_at_ns = Some(published_at_ns);
        }
    }

    pub(crate) fn interrupted_hold_target(&self) -> Vec<f64> {
        self.last_published_target.clone()
    }

    pub(crate) fn final_target_published(&self) -> bool {
        self.final_published_at_ns.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_bus_hold_bounds_accept_twenty_without_changing_motion_limits() {
        let hardware = crate::coordinator::parse_hardware_section_from_toml(
            include_str!("../../../configs/xlerobot-bus2.toml"),
            "right_arm_full_with_base",
        )
        .unwrap();
        let config = crate::coordinator::parse_single_joint_config_from_toml(
            include_str!("../../../configs/xlerobot-bus2-motion.toml"),
            &hardware,
        )
        .unwrap()
        .unwrap();
        let elbow = hardware.motor_ids.iter().position(|&id| id == 3).unwrap();
        let limit = config
            .joints
            .iter()
            .find(|limit| limit.name == hardware.joints[elbow])
            .unwrap();
        let mut positions = vec![0.0; 9];
        for raw in [792, 2998, 3011] {
            positions[elbow] = f64::from(robot_adapter_feetech::raw_to_rad(raw));
            assert!(
                config
                    .validate_hold_positions(&hardware, &positions)
                    .is_ok()
            );
            assert!(!valid_position(positions[elbow], limit));
        }
        for raw in [791, 3012] {
            positions[elbow] = f64::from(robot_adapter_feetech::raw_to_rad(raw));
            assert!(
                config
                    .validate_hold_positions(&hardware, &positions)
                    .is_err()
            );
        }
        let mut zero = hardware.clone();
        zero.calibration_range_tolerances_raw = None;
        positions[elbow] = f64::from(robot_adapter_feetech::raw_to_rad(2998));
        assert!(config.validate_hold_positions(&zero, &positions).is_err());
        zero.calibration_range_tolerances_raw = Some(vec![0; 9]);
        assert!(config.validate_hold_positions(&zero, &positions).is_err());
        for invalid in [vec![], vec![20; 8], vec![4096; 9]] {
            zero.calibration_range_tolerances_raw = Some(invalid);
            assert!(config.validate(&zero).is_err());
        }
    }

    fn hardware() -> HardwareSection {
        HardwareSection {
            id: "测试总线".into(),
            joints: vec!["位置甲".into(), "位置乙".into()],
            motor_ids: vec![1, 2],
            feetech_bus2_mixed_raw: false,
            calibration_range_tolerances_raw: None,
        }
    }

    fn config() -> SingleJointConfig {
        SingleJointConfig {
            max_duration_ms: 1_000,
            settle_timeout_ms: 500,
            stable_window_ms: 100,
            joints: hardware()
                .joints
                .into_iter()
                .map(|name| JointMotionLimits {
                    name,
                    min_position_rad: -2.0,
                    max_position_rad: 2.0,
                    max_velocity_rad_s: 1.0,
                    tolerance_rad: 0.01,
                })
                .collect(),
        }
    }

    #[test]
    fn configuration_requires_exact_joint_coverage() {
        let hardware = hardware();
        let valid = config();
        assert!(valid.validate(&hardware).is_ok());
        let mut reordered = valid.clone();
        reordered.joints.reverse();
        assert!(reordered.validate(&hardware).is_ok());
        let mut missing = valid.clone();
        missing.joints.pop();
        assert!(missing.validate(&hardware).is_err());
        let mut duplicate = valid.clone();
        duplicate.joints.push(valid.joints[0].clone());
        assert!(duplicate.validate(&hardware).is_err());
        let mut unknown = valid;
        unknown.joints[0].name = "未知关节".into();
        assert!(unknown.validate(&hardware).is_err());
    }

    #[test]
    fn configuration_rejects_missing_timing_or_inverted_window() {
        let hardware = hardware();
        for (duration, timeout, window) in [
            (0, 500, 100),
            (1_000, 0, 100),
            (1_000, 500, 0),
            (1_000, 99, 100),
        ] {
            let mut invalid = config();
            invalid.max_duration_ms = duration;
            invalid.settle_timeout_ms = timeout;
            invalid.stable_window_ms = window;
            assert!(invalid.validate(&hardware).is_err());
        }
    }

    #[test]
    fn configuration_requires_stability_window_strictly_before_settle_timeout() {
        let hardware = hardware();
        let mut boundary = config();
        boundary.stable_window_ms = boundary.settle_timeout_ms;
        assert!(boundary.validate(&hardware).is_err());
        boundary.stable_window_ms -= 1;
        assert!(boundary.validate(&hardware).is_ok());
    }

    #[test]
    fn configuration_rejects_nonfinite_limits_and_invalid_ranges() {
        let hardware = hardware();
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            for index in 0..4 {
                let mut invalid = config();
                let joint = &mut invalid.joints[0];
                match index {
                    0 => joint.min_position_rad = value,
                    1 => joint.max_position_rad = value,
                    2 => joint.max_velocity_rad_s = value,
                    _ => joint.tolerance_rad = value,
                }
                assert!(invalid.validate(&hardware).is_err());
            }
        }
        let mut invalid = config();
        invalid.joints[0].min_position_rad = invalid.joints[0].max_position_rad;
        assert!(invalid.validate(&hardware).is_err());
        let mut invalid = config();
        invalid.joints[0].max_velocity_rad_s = 0.0;
        assert!(invalid.validate(&hardware).is_err());
        let mut invalid = config();
        invalid.joints[0].tolerance_rad = -0.001;
        assert!(invalid.validate(&hardware).is_err());
        let mut zero_tolerance = config();
        zero_tolerance.joints[0].tolerance_rad = 0.0;
        assert!(zero_tolerance.validate(&hardware).is_ok());
    }

    #[test]
    fn configuration_rejects_invalid_mixed_layout_and_unknown_fields() {
        let mut mixed = hardware();
        mixed.feetech_bus2_mixed_raw = true;
        assert!(config().validate(&mixed).is_err());
        let text = r"
            max_duration_ms = 1000
            settle_timeout_ms = 500
            stable_window_ms = 100
            joints = []
            guessed_limit = 1
        ";
        assert!(toml::from_str::<SingleJointConfig>(text).is_err());
        let text = r#"
            name = "位置甲"
            min_position_rad = -2.0
            max_position_rad = 2.0
            max_velocity_rad_s = 1.0
            tolerance_rad = 0.01
            guessed_limit = 1
        "#;
        assert!(toml::from_str::<JointMotionLimits>(text).is_err());
    }

    #[test]
    fn mixed_configuration_covers_only_positions_in_configured_slot_order() {
        let mut hardware = crate::coordinator::parse_hardware_section_from_toml(
            include_str!("../../../configs/xlerobot-bus2.toml"),
            "right_arm_full_with_base",
        )
        .unwrap();
        hardware.feetech_bus2_mixed_raw = true;
        hardware.calibration_range_tolerances_raw = None;
        hardware.joints.rotate_left(4);
        hardware.motor_ids.rotate_left(4);
        let mut limits = config();
        limits.joints = hardware
            .joints
            .iter()
            .enumerate()
            .filter(|(slot, _)| !hardware.is_wheel_slot(*slot))
            .map(|(_, name)| JointMotionLimits {
                name: name.clone(),
                ..config().joints[0].clone()
            })
            .collect();
        limits.joints.reverse();
        assert_eq!(limits.joints.len(), 6);
        assert!(limits.validate(&hardware).is_ok());

        let mut missing = limits.clone();
        missing.joints.pop();
        assert!(missing.validate(&hardware).is_err());
        let wheel_name = hardware
            .joints
            .iter()
            .enumerate()
            .find(|(slot, _)| hardware.is_wheel_slot(*slot))
            .unwrap()
            .1;
        let mut wheel_limit = limits.clone();
        wheel_limit.joints.push(JointMotionLimits {
            name: wheel_name.clone(),
            ..limits.joints[0].clone()
        });
        assert!(wheel_limit.validate(&hardware).is_err());
        hardware.motor_ids[0] = hardware.motor_ids[1];
        assert!(limits.validate(&hardware).is_err());
    }
}
