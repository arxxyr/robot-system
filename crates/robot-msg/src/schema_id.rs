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
//! 0x0000_0400..=0x0000_04FF   control 系列（ClearEStopRequest / Response /
//!                              v0.5+ 其它 control RPC payload）
//! 0x0000_0500..=0x7FFF_FFFF   未分配，留给未来扩展
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

// ------------------------ 0x400..0x4FF · control -----------------------------
//
// v0.3 A1 ClearEStop control-plane slice（`docs/handoffs/v0.3_entry.md` §3）+
// v0.3 B4 task control plane（`docs/handoffs/v0.3_b4_outline.md`）。后续
// v0.5+ 其它 §5.3 RPC payload 也分配本段。

/// `robot.msg.ClearEStopRequest` —— v0.3 A1 `ClearEStop` control action 跨
/// 进程 wire format（iceoryx2 `RequestResponse` client → server payload）。
pub const SCHEMA_CLEAR_ESTOP_REQUEST: u32 = 0x0000_0400;
/// `robot.msg.ClearEStopResponse` —— v0.3 A1 `ClearEStop` control action 跨
/// 进程 wire format（iceoryx2 `RequestResponse` server → client reply payload）。
pub const SCHEMA_CLEAR_ESTOP_RESPONSE: u32 = 0x0000_0401;

// ----- v0.3 B4 task control plane（`docs/handoffs/v0.3_b4_outline.md`）-----
//
// 4 个 RR 一对 request/response（task_invoke / task_status / task_cancel /
// clear_error）+ 1 个 pubsub event envelope = 9 个 schema_id。

/// `robot.msg.InvokeTaskRequest` —— B4 `InvokeTask` RR client → s1 payload。
pub const SCHEMA_INVOKE_TASK_REQUEST: u32 = 0x0000_0402;
/// `robot.msg.InvokeTaskResponse` —— B4 `InvokeTask` RR s1 → client reply。
pub const SCHEMA_INVOKE_TASK_RESPONSE: u32 = 0x0000_0403;
/// `robot.msg.GetTaskStatusRequest` —— B4 `GetTaskStatus` RR client → s1 payload。
pub const SCHEMA_GET_TASK_STATUS_REQUEST: u32 = 0x0000_0404;
/// `robot.msg.GetTaskStatusResponse` —— B4 `GetTaskStatus` RR s1 → client reply。
pub const SCHEMA_GET_TASK_STATUS_RESPONSE: u32 = 0x0000_0405;
/// `robot.msg.CancelTaskRequest` —— B4 `CancelTask` RR client → s1 payload。
pub const SCHEMA_CANCEL_TASK_REQUEST: u32 = 0x0000_0406;
/// `robot.msg.CancelTaskResponse` —— B4 `CancelTask` RR s1 → client reply。
pub const SCHEMA_CANCEL_TASK_RESPONSE: u32 = 0x0000_0407;
/// `robot.msg.ClearErrorRequest` —— B4 `ClearError` RR client → s1 payload。
pub const SCHEMA_CLEAR_ERROR_REQUEST: u32 = 0x0000_0408;
/// `robot.msg.ClearErrorResponse` —— B4 `ClearError` RR s1 → client reply。
pub const SCHEMA_CLEAR_ERROR_RESPONSE: u32 = 0x0000_0409;
/// `robot.msg.ActionEventEnvelope` —— B4 `StreamTaskEvents` per-ActionId
/// pubsub topic payload（**不**走 RR；详 `schemas/action_event.fbs`）。
pub const SCHEMA_ACTION_EVENT_ENVELOPE: u32 = 0x0000_040A;

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
    (SCHEMA_CLEAR_ESTOP_REQUEST, "robot.msg.ClearEStopRequest"),
    (SCHEMA_CLEAR_ESTOP_RESPONSE, "robot.msg.ClearEStopResponse"),
    (SCHEMA_INVOKE_TASK_REQUEST, "robot.msg.InvokeTaskRequest"),
    (SCHEMA_INVOKE_TASK_RESPONSE, "robot.msg.InvokeTaskResponse"),
    (
        SCHEMA_GET_TASK_STATUS_REQUEST,
        "robot.msg.GetTaskStatusRequest",
    ),
    (
        SCHEMA_GET_TASK_STATUS_RESPONSE,
        "robot.msg.GetTaskStatusResponse",
    ),
    (SCHEMA_CANCEL_TASK_REQUEST, "robot.msg.CancelTaskRequest"),
    (SCHEMA_CANCEL_TASK_RESPONSE, "robot.msg.CancelTaskResponse"),
    (SCHEMA_CLEAR_ERROR_REQUEST, "robot.msg.ClearErrorRequest"),
    (SCHEMA_CLEAR_ERROR_RESPONSE, "robot.msg.ClearErrorResponse"),
    (
        SCHEMA_ACTION_EVENT_ENVELOPE,
        "robot.msg.ActionEventEnvelope",
    ),
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
