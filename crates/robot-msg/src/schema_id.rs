//! `schema_id` 全局常量表与 `SCHEMA_REGISTRY`。
//!
//! 对应 `docs/00_design.md` §2.2：`FrameHeader.schema_id` 路由到 body root table。
//! 本模块是**唯一**的 `schema_id` 分配来源；新增 schema 必须在此加 `pub const` +
//! `SCHEMA_REGISTRY` 条目，并通过 `schema_lint` 静态校验（唯一、未与 deprecated
//! 冲突）。
//!
//! ### 分段
//!
//! ```text
//! 0x0000_0000           保留（FrameHeader 本身不走 schema_id；0 视为未设置）
//! 0x0000_0001..=0x0000_00FF   joint 系列（JointLayout / JointState / JointCommand）
//! 0x0000_0100..=0x0000_01FF   motion 系列（Twist / Odometry / …）
//! 0x0000_0200..=0x0000_02FF   sensor 系列（Image / PointCloud / Tensor / …）
//! 0x0000_0300..=0x0000_03FF   event 系列（EStop / Heartbeat / …）
//! 0x0000_0400..=0x7FFF_FFFF   未分配，留给未来扩展
//! 0x8000_0000..=0xFFFF_FFFF   实验 / fork 命名空间（下游机器人可用，上游不占）
//! ```
//!
//! 新增 schema 时优先在对应段内找下一个空闲号；跨段必须在本文件 doc 注释同步
//! 调整分段。deprecated `schema_id` **不得复用**（`schema_lint` 拦截）。

/// 预留的"无效"占位：FrameHeader 未写 body schema 时（理论不该出现）。
pub const SCHEMA_UNSPECIFIED: u32 = 0x0000_0000;

// ------------------------ 0x01..0xFF · joint ---------------------------------

/// `robot.msg.JointLayout`。
pub const SCHEMA_JOINT_LAYOUT: u32 = 0x0000_0001;
/// `robot.msg.JointState`。
pub const SCHEMA_JOINT_STATE: u32 = 0x0000_0002;
/// `robot.msg.JointCommand`。
pub const SCHEMA_JOINT_COMMAND: u32 = 0x0000_0003;

// ------------------------ 0x100..0x1FF · motion ------------------------------

/// `robot.msg.Twist`。
pub const SCHEMA_TWIST: u32 = 0x0000_0100;

// ------------------------ 0x200..0x2FF · sensor ------------------------------

/// `robot.msg.ImageHeader`。
pub const SCHEMA_IMAGE_HEADER: u32 = 0x0000_0200;
/// `robot.msg.PointCloudHeader`。
pub const SCHEMA_POINTCLOUD_HEADER: u32 = 0x0000_0201;
/// `robot.msg.TensorHeader`。
pub const SCHEMA_TENSOR_HEADER: u32 = 0x0000_0202;

// ------------------------ 0x300..0x3FF · event -------------------------------

/// `robot.msg.EStopEvent`。
pub const SCHEMA_ESTOP_EVENT: u32 = 0x0000_0300;
/// `robot.msg.Heartbeat`。
pub const SCHEMA_HEARTBEAT: u32 = 0x0000_0301;

// -----------------------------------------------------------------------------
// deprecated 登记
// -----------------------------------------------------------------------------

/// 曾经使用过、现在不得复用的 `schema_id`。`schema_lint` 会读这张表，拒绝把
/// `SCHEMA_REGISTRY` 里某条 id 复用到此。
///
/// v0.2 开工无历史包袱；登记器留空。新版淘汰老 table 时往这里补。
pub const DEPRECATED_SCHEMA_IDS: &[(u32, &str)] = &[
    // (0x0000_00XX, "robot.msg.OldTable, 移除于 vX.Y"),
];

// -----------------------------------------------------------------------------
// Registry
// -----------------------------------------------------------------------------

/// `SCHEMA_REGISTRY` 是 `(schema_id, canonical_name)` 的有序列表；订阅侧按
/// `schema_id` 查名用于诊断 / 日志 / 未知类型拒收。
///
/// **排序约束**：按 `schema_id` 升序，`schema_lint` 静态校验唯一性 + 单调。
pub const SCHEMA_REGISTRY: &[(u32, &str)] = &[
    (SCHEMA_JOINT_LAYOUT, "robot.msg.JointLayout"),
    (SCHEMA_JOINT_STATE, "robot.msg.JointState"),
    (SCHEMA_JOINT_COMMAND, "robot.msg.JointCommand"),
    (SCHEMA_TWIST, "robot.msg.Twist"),
    (SCHEMA_IMAGE_HEADER, "robot.msg.ImageHeader"),
    (SCHEMA_POINTCLOUD_HEADER, "robot.msg.PointCloudHeader"),
    (SCHEMA_TENSOR_HEADER, "robot.msg.TensorHeader"),
    (SCHEMA_ESTOP_EVENT, "robot.msg.EStopEvent"),
    (SCHEMA_HEARTBEAT, "robot.msg.Heartbeat"),
];

/// 按 `schema_id` 查规范名；未知返回 `None`。
#[must_use]
pub fn lookup_name(schema_id: u32) -> Option<&'static str> {
    SCHEMA_REGISTRY
        .iter()
        .find(|(id, _)| *id == schema_id)
        .map(|(_, name)| *name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_ids_are_unique_and_monotonic() {
        let mut prev: Option<u32> = None;
        for (id, _) in SCHEMA_REGISTRY {
            if let Some(p) = prev {
                assert!(
                    *id > p,
                    "SCHEMA_REGISTRY not strictly ascending at {id:#010x}"
                );
            }
            prev = Some(*id);
        }
    }

    #[test]
    fn registry_does_not_collide_with_deprecated() {
        for (id, _) in SCHEMA_REGISTRY {
            assert!(
                !DEPRECATED_SCHEMA_IDS.iter().any(|(d, _)| d == id),
                "schema_id {id:#010x} is deprecated and cannot be reused"
            );
        }
    }

    #[test]
    fn lookup_roundtrip() {
        assert_eq!(
            lookup_name(SCHEMA_JOINT_STATE),
            Some("robot.msg.JointState")
        );
        assert_eq!(lookup_name(SCHEMA_HEARTBEAT), Some("robot.msg.Heartbeat"));
        assert_eq!(lookup_name(0xDEAD_BEEF), None);
        assert_eq!(lookup_name(SCHEMA_UNSPECIFIED), None);
    }
}
