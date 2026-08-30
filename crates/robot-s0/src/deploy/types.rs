//! 部署侧基础类型（v0.5 候选 E 段(9)，Spec P.2.9 / R9-1）。
//!
//! **`DeployArm` 在 robot-s0 本地定义**：全仓唯一的 `Side` 住
//! `robot-kinematics-xlerobot`（`lib.rs`），robot-s0 **不依赖该 crate**
//! （`Cargo.toml` 实证）且**不得**为部署件新增这条依赖边（R9-1）。

use std::fmt;

/// 部署的一条臂（= 一条物理 bus）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeployArm {
    /// bus1：6 臂关节 + 2 head（ID 1-8）。
    Left,
    /// bus2：6 臂关节 + 3 wheels（ID 1-9）。
    Right,
}

impl DeployArm {
    /// CLI / 日志短名。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
        }
    }

    /// CLI 解析（`left` / `right`）。
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "left" => Some(Self::Left),
            "right" => Some(Self::Right),
            _ => None,
        }
    }

    /// 本 bus 上**只需断扭**的电机 ID（`Torque_Enable = 0`）。
    ///
    /// - Left = bus1 ID 1-8（6 臂 + 2 head）；
    /// - Right = bus2 ID 1-6（6 臂；ID 7-9 是轮子，见
    ///   [`Self::velocity_then_torque_ids`]）。
    #[must_use]
    pub const fn torque_only_ids(self) -> &'static [u8] {
        match self {
            Self::Left => &[1, 2, 3, 4, 5, 6, 7, 8],
            Self::Right => &[1, 2, 3, 4, 5, 6],
        }
    }

    /// 本 bus 上需要**先零速再断扭**的电机 ID（轮子；顺序锁定 D13 第 2 步）。
    #[must_use]
    pub const fn velocity_then_torque_ids(self) -> &'static [u8] {
        match self {
            Self::Left => &[],
            Self::Right => &[7, 8, 9],
        }
    }

    /// 本 bus 的全部电机 ID（审计 / 日志用）。
    #[must_use]
    pub fn all_ids(self) -> Vec<u8> {
        let mut ids = self.torque_only_ids().to_vec();
        ids.extend_from_slice(self.velocity_then_torque_ids());
        ids
    }

    /// 本 bus 的**安全写条数**（R10-2 精确计数）：
    ///
    /// - Left  = 8（`[T1..T8]`）；
    /// - Right = 12（`[T1..T6, V7,T7, V8,T8, V9,T9]`）。
    #[must_use]
    pub const fn safe_write_count(self) -> usize {
        match self {
            Self::Left => 8,
            Self::Right => 12,
        }
    }
}

impl fmt::Display for DeployArm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// handoff（双 bus）合计安全写条数 = 左 8 + 右 12（R10-2）。
pub const HANDOFF_WRITE_COUNT: usize = 20;

/// 安全寄存器名（feetech 控制表字面；`registers.rs` 实证）。
pub const REG_TORQUE_ENABLE: &str = "Torque_Enable";
pub const REG_GOAL_VELOCITY: &str = "Goal_Velocity";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arm_parse_and_label_round_trip() {
        assert_eq!(DeployArm::parse("left"), Some(DeployArm::Left));
        assert_eq!(DeployArm::parse("right"), Some(DeployArm::Right));
        assert_eq!(DeployArm::parse("LEFT"), None);
        assert_eq!(DeployArm::parse(""), None);
        assert_eq!(DeployArm::Left.label(), "left");
        assert_eq!(DeployArm::Right.to_string(), "right");
    }

    /// D13 精确 ID 集（round 8 P0-2 扩程 + R10-2 计数）——本表是 AC-E16 的
    /// 数值权威，改动必须同步 Spec。
    #[test]
    fn motor_id_sets_match_d13() {
        assert_eq!(
            DeployArm::Left.torque_only_ids(),
            &[1, 2, 3, 4, 5, 6, 7, 8],
            "bus1 = 6 臂 + 2 head，全部只断扭"
        );
        assert!(
            DeployArm::Left.velocity_then_torque_ids().is_empty(),
            "bus1 无轮子，不该有先零速再断扭项：{:?}",
            DeployArm::Left.velocity_then_torque_ids()
        );
        assert_eq!(DeployArm::Right.torque_only_ids(), &[1, 2, 3, 4, 5, 6]);
        assert_eq!(
            DeployArm::Right.velocity_then_torque_ids(),
            &[7, 8, 9],
            "bus2 轮子必须先零速再断扭"
        );
        assert_eq!(DeployArm::Left.all_ids().len(), 8);
        assert_eq!(DeployArm::Right.all_ids().len(), 9);
    }

    #[test]
    fn safe_write_counts_are_8_and_12() {
        assert_eq!(DeployArm::Left.safe_write_count(), 8);
        assert_eq!(DeployArm::Right.safe_write_count(), 12);
        assert_eq!(
            DeployArm::Left.safe_write_count() + DeployArm::Right.safe_write_count(),
            HANDOFF_WRITE_COUNT
        );
        // 计数自洽：只断扭 1 条 / 先零速再断扭 2 条。
        for arm in [DeployArm::Left, DeployArm::Right] {
            let expected = arm.torque_only_ids().len() + arm.velocity_then_torque_ids().len() * 2;
            assert_eq!(arm.safe_write_count(), expected, "{arm}");
        }
    }
}
