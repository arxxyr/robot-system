//! `XLeRobot` 内部总线目标入口；共用原 `SerialRT` writer、整总线启停与反馈。
//! 每总线仍只有一个命令流、控制权和停止域；左总线选择 Position 组，右总线
//! 使用一个完整的位置/raw速度目标，不提供组级超时或并发多生产者。

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

const BUS2_JOINTS: [&str; 9] = [
    "right_arm_shoulder_pan",
    "right_arm_shoulder_lift",
    "right_arm_elbow_flex",
    "right_arm_wrist_flex",
    "right_arm_wrist_roll",
    "right_arm_gripper",
    "base_front_wheel",
    "base_left_wheel",
    "base_right_wheel",
];

/// 右总线同一完整目标：右臂位置与三轮 raw 速度共享一个来源期限。
/// 本类型只提供配置映射；设备身份与启动权限由原 S0 入口守门。
#[derive(Clone, Copy, Debug)]
pub struct XleRobotBus2Layout {
    slots: [usize; 9],
}

impl XleRobotBus2Layout {
    /// # Errors
    /// 右总线九关节的名称/ID 缺失、重复或错配。
    pub fn from_hardware(hardware: &HardwareSection) -> Result<Self, RobotError> {
        if hardware.id != "right_arm_full_with_base"
            || hardware.adapter != "feetech_sts3215"
            || hardware.joints.len() != 9
            || hardware.motor_ids.len() != 9
        {
            return Err(RobotError::InvalidParameter("需要右臂和底盘完整九关节配置"));
        }
        let mut slots = [0; 9];
        let mut seen = 0_u16;
        let mut seen_ids = 0_u16;
        for (slot, (name, &id)) in hardware.joints.iter().zip(&hardware.motor_ids).enumerate() {
            let Some(logical) = BUS2_JOINTS
                .iter()
                .position(|&expected_name| name == expected_name)
            else {
                return Err(RobotError::InvalidParameter("右总线关节名称与电机ID不匹配"));
            };
            if (logical < 6 && usize::from(id) != logical + 1)
                || (logical >= 6 && !(7..=9).contains(&id))
            {
                return Err(RobotError::InvalidParameter("右总线关节名称与电机ID不匹配"));
            }
            if seen & (1 << logical) != 0 || seen_ids & (1 << id) != 0 {
                return Err(RobotError::InvalidParameter("右总线关节重复"));
            }
            seen |= 1 << logical;
            seen_ids |= 1 << id;
            slots[logical] = slot;
        }
        Ok(Self { slots })
    }

    /// 将 wire 的完整配置槽位解回既有混合目标，拒绝跨组字段及未批准 raw。
    pub(crate) fn command_from_config_slots(
        &self,
        positions: [f64; 9],
        raw_velocities: [i16; 9],
        seq: u64,
        stamp_ns: u64,
        deadline_ns: u64,
    ) -> Result<JointCommandBuf, RobotError> {
        if self.slots[..6]
            .iter()
            .any(|&slot| raw_velocities[slot] != 0)
            || self.slots[6..]
                .iter()
                .any(|&slot| positions[slot] != 0.0 || !matches!(raw_velocities[slot], 0 | 500))
        {
            return Err(RobotError::InvalidParameter(
                "混合wire的槽位字段或raw速度无效",
            ));
        }
        #[allow(clippy::cast_possible_truncation)]
        let arm = std::array::from_fn(|logical| positions[self.slots[logical]] as f32);
        let wheels = std::array::from_fn(|logical| raw_velocities[self.slots[logical + 6]]);
        self.command(arm, wheels, seq, stamp_ns, deadline_ns)
    }

    /// 臂位置按 ID1..6、轮 raw 按前/左/右角色顺序；不混同为 rad/s。
    /// 三轮上限另由显式 Feetech profile 在首个动作写入前统一验证。
    /// # Errors
    /// 位置不有限、raw 为 `i16::MIN` 或时间窗口无效。
    pub fn command(
        &self,
        arm: [f32; 6],
        wheels_raw: [i16; 3],
        seq: u64,
        stamp_ns: u64,
        deadline_ns: u64,
    ) -> Result<JointCommandBuf, RobotError> {
        let mut command = JointCommandBuf {
            mode: ControlModeRepr::Position,
            joint_count: 9,
            seq,
            stamp_ns,
            deadline_ns,
            selection: JointSelection::MixedPositionRawVelocity {
                position_mask: self.slots[..6]
                    .iter()
                    .fold(0, |mask, slot| mask | (1 << slot)),
                velocity_mask: self.slots[6..]
                    .iter()
                    .fold(0, |mask, slot| mask | (1 << slot)),
            },
            ..JointCommandBuf::default()
        };
        for (&slot, target) in self.slots[..6].iter().zip(arm) {
            command.positions[slot] = target;
        }
        for (&slot, target) in self.slots[6..].iter().zip(wheels_raw) {
            command.raw_velocities[slot] = target;
        }
        command.as_view().validate_selection(9)?;
        Ok(command)
    }
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

    fn bus2_fixture() -> HardwareSection {
        let mut config: crate::config_loader::RobotConfigToml =
            toml::from_str(include_str!("../../../configs/xlerobot_bus2_full.toml")).unwrap();
        let mut hardware = config.hardware.remove(0);
        // 黄金槽位属于本测试夹具，实际配置的六种映射另行验证。
        hardware.joints = [
            "right_arm_shoulder_pan",
            "right_arm_shoulder_lift",
            "right_arm_elbow_flex",
            "right_arm_wrist_flex",
            "right_arm_wrist_roll",
            "right_arm_gripper",
            "base_right_wheel",
            "base_front_wheel",
            "base_left_wheel",
        ]
        .map(str::to_owned)
        .to_vec();
        hardware.motor_ids = vec![1, 2, 3, 4, 5, 6, 7, 8, 9];
        hardware
    }

    #[test]
    fn mixed_bus2_mapping_preserves_front_left_right_raw_order_after_config_reorder() {
        let hardware = bus2_fixture();
        assert_eq!(hardware.motor_ids, [1, 2, 3, 4, 5, 6, 7, 8, 9]);
        assert_eq!(
            &hardware.joints[6..],
            &["base_right_wheel", "base_front_wheel", "base_left_wheel"]
        );
        let normal = XleRobotBus2Layout::from_hardware(&hardware)
            .unwrap()
            .command([1., 2., 3., 4., 5., 6.], [100, -200, 300], 81, 100, 200)
            .unwrap();
        assert_eq!(&normal.raw_velocities[6..9], &[300, 100, -200]);
        let mut reordered = hardware.clone();
        reordered.joints.reverse();
        reordered.motor_ids.reverse();
        let layout = XleRobotBus2Layout::from_hardware(&reordered).unwrap();
        let command = layout
            .command([1., 2., 3., 4., 5., 6.], [100, -200, 300], 81, 100, 200)
            .unwrap();
        assert_eq!(
            command.selection,
            JointSelection::MixedPositionRawVelocity {
                position_mask: 0x1f8,
                velocity_mask: 7
            }
        );
        assert_eq!(&command.positions[3..9], &[6., 5., 4., 3., 2., 1.]);
        assert_eq!(&command.raw_velocities[..3], &[-200, 100, 300]);
        assert_eq!(
            (command.seq, command.stamp_ns, command.deadline_ns),
            (81, 100, 200)
        );
        for variant in 0..4 {
            let mut invalid = reordered.clone();
            match variant {
                0 => invalid.id = "left_arm_full_with_head".into(),
                1 => invalid.joints[0] = "head_motor_2".into(),
                2 => invalid.motor_ids[0] = invalid.motor_ids[1],
                _ => {
                    invalid.joints.pop();
                    invalid.motor_ids.pop();
                }
            }
            assert!(XleRobotBus2Layout::from_hardware(&invalid).is_err());
        }
        assert!(
            layout
                .command([0.; 6], [0, i16::MIN, 0], 1, 100, 200)
                .is_err()
        );
        let mut remapped = hardware;
        remapped.motor_ids.swap(6, 8);
        assert!(XleRobotBus2Layout::from_hardware(&remapped).is_ok());
        remapped.joints[6] = remapped.joints[8].clone();
        assert!(XleRobotBus2Layout::from_hardware(&remapped).is_err());
    }

    #[test]
    fn mixed_bus2_uses_configured_slots_for_every_wheel_id_permutation() {
        let mut config: crate::config_loader::RobotConfigToml =
            toml::from_str(include_str!("../../../configs/xlerobot_bus2_full.toml")).unwrap();
        let original = config.hardware.remove(0);
        for [front, left, right] in [
            [7, 8, 9],
            [7, 9, 8],
            [8, 7, 9],
            [8, 9, 7],
            [9, 7, 8],
            [9, 8, 7],
        ] {
            let mut hardware = original.clone();
            for (role, motor_id) in [
                ("base_front_wheel", front),
                ("base_left_wheel", left),
                ("base_right_wheel", right),
            ] {
                let slot = hardware
                    .joints
                    .iter()
                    .position(|name| name == role)
                    .unwrap();
                hardware.motor_ids[slot] = motor_id;
            }
            for reordered in [false, true] {
                let mut hardware = hardware.clone();
                if reordered {
                    hardware.joints.reverse();
                    hardware.motor_ids.reverse();
                }
                let command = XleRobotBus2Layout::from_hardware(&hardware)
                    .unwrap()
                    .command([0.; 6], [100, -200, 300], 3, 10, 20)
                    .unwrap();
                for (role, expected) in [
                    ("base_front_wheel", 100),
                    ("base_left_wheel", -200),
                    ("base_right_wheel", 300),
                ] {
                    let slot = hardware
                        .joints
                        .iter()
                        .position(|name| name == role)
                        .unwrap();
                    assert_eq!(command.raw_velocities[slot], expected);
                }
            }
        }
        let front = original
            .joints
            .iter()
            .position(|name| name == "base_front_wheel")
            .unwrap();
        let left = original
            .joints
            .iter()
            .position(|name| name == "base_left_wheel")
            .unwrap();
        let arm = original
            .joints
            .iter()
            .position(|name| name == "right_arm_shoulder_pan")
            .unwrap();
        for variant in 0..4 {
            let mut invalid = original.clone();
            match variant {
                0 => invalid.joints[front] = invalid.joints[left].clone(),
                1 => invalid.motor_ids[front] = invalid.motor_ids[left],
                2 => invalid.motor_ids[front] = 10,
                _ => invalid.motor_ids[arm] = 2,
            }
            assert!(XleRobotBus2Layout::from_hardware(&invalid).is_err());
        }
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
