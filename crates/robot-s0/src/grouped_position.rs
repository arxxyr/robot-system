//! 左总线的最小分组位置入口；共用原 `SerialRT` writer、整总线启停与反馈。
//! 每总线仍只有一个命令流、控制权和停止域；selection 只缩小本次 Position
//! 写入，未选中关节保持设备既有目标，不提供组级超时或并发多生产者。

use robot_core::adapter::{ControlModeRepr, JointCommandBuf, JointSelection};
use robot_core::error::RobotError;

use crate::config_loader::HardwareSection;

const BUS1_JOINTS: [(&str, u8); 8] = [
    ("left_arm_shoulder_pan", 1),
    ("left_arm_shoulder_lift", 2),
    ("left_arm_elbow_flex", 3),
    ("left_arm_wrist_flex", 4),
    ("left_arm_wrist_roll", 5),
    ("left_arm_gripper", 6),
    ("head_motor_1", 7),
    ("head_motor_2", 8),
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Bus1PositionGroup {
    LeftArm,
    Head,
}

/// 配置槽位映射，不拥有总线、不保存目标；只有已核对的左臂和头部布局可构造。
/// 物理串口身份、配置身份和运行权限仍由原 S0 启动与 `SerialRT` 路径守门。
#[derive(Clone, Copy, Debug)]
pub struct XleRobotBus1Layout {
    slots: [usize; 8],
}

impl XleRobotBus1Layout {
    /// # Errors
    /// 不是左总线八关节配置，或名称/ID 不匹配、重复、缺失。
    pub fn from_hardware(hardware: &HardwareSection) -> Result<Self, RobotError> {
        if hardware.id != "left_arm_full_with_head"
            || hardware.adapter != "feetech_sts3215"
            || hardware.joints.len() != 8
            || hardware.motor_ids.len() != 8
        {
            return Err(RobotError::InvalidParameter("需要左总线臂和头部的完整配置"));
        }
        let mut slots = [0; 8];
        let mut seen = 0_u32;
        for (slot, (name, &id)) in hardware.joints.iter().zip(&hardware.motor_ids).enumerate() {
            let Some(logical) = BUS1_JOINTS
                .iter()
                .position(|&(expected_name, expected_id)| {
                    name == expected_name && id == expected_id
                })
            else {
                return Err(RobotError::InvalidParameter("左总线关节名称与电机ID不匹配"));
            };
            let bit = 1 << logical;
            if seen & bit != 0 {
                return Err(RobotError::InvalidParameter("左总线关节重复"));
            }
            seen |= bit;
            slots[logical] = slot;
        }
        Ok(Self { slots })
    }

    /// 构造可交给原 `LatestCommandSlot` 的完整总线维度 Position 帧。
    /// 目标顺序按组内电机 ID：左臂 1..6、头部 7/8；未选中槽位不会写入。
    /// 本入口不 Enable，不绕过 `SerialTick` 的租约、时间或故障收敛检查。
    ///
    /// # Errors
    /// 组内目标维度、有限值或时间窗口无效。
    pub fn command(
        &self,
        group: Bus1PositionGroup,
        positions: &[f32],
        seq: u64,
        stamp_ns: u64,
        deadline_ns: u64,
    ) -> Result<JointCommandBuf, RobotError> {
        let logical_slots = match group {
            Bus1PositionGroup::LeftArm => &self.slots[..6],
            Bus1PositionGroup::Head => &self.slots[6..],
        };
        if positions.len() != logical_slots.len() {
            return Err(RobotError::InvalidParameter("分组位置目标维度无效"));
        }
        let mut command = JointCommandBuf {
            mode: ControlModeRepr::Position,
            stamp_ns,
            deadline_ns,
            seq,
            joint_count: 8,
            ..JointCommandBuf::default()
        };
        let mut mask = 0;
        for (&slot, &target) in logical_slots.iter().zip(positions) {
            mask |= 1 << slot;
            command.positions[slot] = target;
        }
        command.selection = JointSelection::Mask(mask);
        command.as_view().validate_selection(8)?;
        Ok(command)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn hardware() -> HardwareSection {
        let mut config: crate::config_loader::RobotConfigToml =
            toml::from_str(include_str!("../../../configs/xlerobot_bus1_full.toml")).unwrap();
        config.hardware.remove(0)
    }

    #[test]
    fn maps_group_targets_by_configured_name_and_id_in_any_order() {
        let mut config = hardware();
        config.joints.reverse();
        config.motor_ids.reverse();
        let layout = XleRobotBus1Layout::from_hardware(&config).unwrap();
        let head = layout
            .command(Bus1PositionGroup::Head, &[0.1, 0.2], 7, 10, 20)
            .unwrap();
        assert_eq!(head.selection, JointSelection::Mask(0b11));
        assert_eq!(&head.positions[..2], &[0.2, 0.1]);
        assert_eq!(head.joint_count, 8);
        let arm = layout
            .command(
                Bus1PositionGroup::LeftArm,
                &[1., 2., 3., 4., 5., 6.],
                8,
                10,
                20,
            )
            .unwrap();
        assert_eq!(arm.selection, JointSelection::Mask(0b1111_1100));
        assert_eq!(&arm.positions[2..8], &[6., 5., 4., 3., 2., 1.]);
    }

    #[test]
    fn rejects_wrong_bus_mapping_and_invalid_targets() {
        let original = hardware();
        for variant in 0..5 {
            let mut config = original.clone();
            match variant {
                0 => config.id = "right_arm_full_with_base".into(),
                1 => config.joints[6] = "base_left_wheel".into(),
                2 => config.motor_ids[6] = 8,
                3 => {
                    config.joints[7] = config.joints[6].clone();
                    config.motor_ids[7] = 7;
                }
                _ => {
                    config.joints.pop();
                    config.motor_ids.pop();
                }
            }
            assert!(XleRobotBus1Layout::from_hardware(&config).is_err());
        }
        let layout = XleRobotBus1Layout::from_hardware(&original).unwrap();
        for (targets, stamp, deadline) in [
            (&[0.1][..], 1, 2),
            (&[0., f32::NAN][..], 1, 2),
            (&[0., 0.][..], 2, 2),
        ] {
            assert!(
                layout
                    .command(Bus1PositionGroup::Head, targets, 1, stamp, deadline)
                    .is_err()
            );
        }
    }
}
