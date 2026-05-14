//! Schema marker 类型 + [`BodyMarker`] trait。
//!
//! `FlatBuffers` 生成的 root table（`msg::JointState<'a>` 等）带生命周期参数，
//! 不能直接作为 `SampleLease<T>` / `LatestSubscriber<T>` 的 `T = 'static` 类型
//! 参数使用。本模块提供**空结构体 marker**（[`JointState`] / [`JointCommand`]
//! / …）作为 `T`，通过 [`BodyMarker`] trait 的 GAT `Root<'a>` 关联真正的
//! flatc 生成类型。
//!
//! ### 典型用法
//!
//! ```ignore
//! use robot_msg::markers::{self, BodyMarker};
//! fn verify_body<M: BodyMarker>(bytes: &[u8]) -> Result<M::Root<'_>, _> {
//!     flatbuffers::root::<M::Root<'_>>(bytes)
//! }
//! let _js: robot_msg::msg::JointState<'_> = verify_body::<markers::JointState>(body)?;
//! ```
//!
//! ### 新增 schema 时
//!
//! 1. `src/schema_id.rs` 追加常量 + `SCHEMA_REGISTRY` 条目
//! 2. 本文件追加 `pub struct NewThing;` + `impl BodyMarker for NewThing { ... }`
//! 3. 若带 blob（body 里有 `blob: BlobDesc` 字段），`BLOB_EMBEDDED = true` +
//!    覆盖 `read_blob_desc()`

use crate::msg;
use crate::schema_id;

/// `BlobDesc` 从 typed body 读出的只读视图。
///
/// 从 body 的 `FlatBuffers` `BlobDesc` table 抽出 parser 需要的字段（对齐 / 大小 /
/// 偏移 / dtype），避免 `robot-transport::parser` 深入每个 schema 的具体类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlobDescInfo {
    /// `BlobDesc.alignment` —— 要求 `blob_offset % alignment == 0`。
    pub alignment: u16,
    /// `BlobDesc.size` —— v0.1.2 固定等于 `FrameHeader.blob_size`（§2.2）。
    pub size: u64,
    /// `BlobDesc.offset` —— v0.1.2 固定为 0（§2.2 单 blob 约束）。
    pub offset: u64,
    /// `BlobDesc.dtype` 裸字节值（0=U8 / 1=I8 / … 见 `schemas/blob.fbs`）。
    pub dtype_raw: i8,
}

/// Schema marker trait：把一个空 struct（`markers::JointState`）绑定到
/// 一条 `SCHEMA_REGISTRY` 记录 + 一个 flatc 生成的 root table。
///
/// `'static` / `Send` / `Sync` 约束来自 marker 自身（空 struct 自动满足），
/// 而非 `Root<'a>` —— `Root<'a>` 借用输入 slice，不是 `'static`。
pub trait BodyMarker: 'static + Send + Sync {
    /// 对应 `schema_id::SCHEMA_REGISTRY` 里的 id。
    const SCHEMA_ID: u32;

    /// `SCHEMA_REGISTRY` 里的规范名，用来在错误消息里回显。
    const SCHEMA_NAME: &'static str;

    /// 该 body table 是否在内部嵌入 `BlobDesc`（`ImageHeader` /
    /// `PointCloudHeader` / `TensorHeader` 为 `true`；其余 `false`）。
    const BLOB_EMBEDDED: bool;

    /// flatc 生成的 root table 类型；GAT 让它借用 parser 的输入 slice。
    type Root<'a>: flatbuffers::Follow<'a, Inner = Self::Root<'a>> + flatbuffers::Verifiable + Copy;

    /// 从 verified body 读 `BlobDesc`；默认返回 `None`（`BLOB_EMBEDDED = false`
    /// 的 marker 沿用默认）。带 blob 的 marker 覆盖此方法。
    fn read_blob_desc(_root: &Self::Root<'_>) -> Option<BlobDescInfo> {
        None
    }
}

// ---------------------------------------------------------------------------
// 9 个 marker（顺序对齐 schema_id::SCHEMA_REGISTRY）
// ---------------------------------------------------------------------------

/// Marker for [`msg::JointLayout`]。
#[derive(Debug, Clone, Copy, Default)]
pub struct JointLayout;

/// Marker for [`msg::JointState`]。
#[derive(Debug, Clone, Copy, Default)]
pub struct JointState;

/// Marker for [`msg::JointCommand`]。
#[derive(Debug, Clone, Copy, Default)]
pub struct JointCommand;

/// Marker for [`msg::Twist`]。
#[derive(Debug, Clone, Copy, Default)]
pub struct Twist;

/// Marker for [`msg::ImageHeader`]。
#[derive(Debug, Clone, Copy, Default)]
pub struct ImageHeader;

/// Marker for [`msg::PointCloudHeader`]。
#[derive(Debug, Clone, Copy, Default)]
pub struct PointCloudHeader;

/// Marker for [`msg::TensorHeader`]。
#[derive(Debug, Clone, Copy, Default)]
pub struct TensorHeader;

/// Marker for [`msg::EStopEvent`]。
#[derive(Debug, Clone, Copy, Default)]
pub struct EStopEvent;

/// Marker for [`msg::Heartbeat`]。
#[derive(Debug, Clone, Copy, Default)]
pub struct Heartbeat;

/// Marker for [`msg::ClearEStopRequest`]（v0.3 A1 `ClearEStop` control-plane
/// slice request payload）。
#[derive(Debug, Clone, Copy, Default)]
pub struct ClearEStopRequest;

/// Marker for [`msg::ClearEStopResponse`]（v0.3 A1 `ClearEStop` control-plane
/// slice response payload）。
#[derive(Debug, Clone, Copy, Default)]
pub struct ClearEStopResponse;

/// Marker for [`msg::InvokeTaskRequest`]（v0.3 B4 `InvokeTask` RR payload）。
#[derive(Debug, Clone, Copy, Default)]
pub struct InvokeTaskRequest;

/// Marker for [`msg::InvokeTaskResponse`]（v0.3 B4 `InvokeTask` RR reply）。
#[derive(Debug, Clone, Copy, Default)]
pub struct InvokeTaskResponse;

/// Marker for [`msg::GetTaskStatusRequest`]（v0.3 B4 `GetTaskStatus` RR payload）。
#[derive(Debug, Clone, Copy, Default)]
pub struct GetTaskStatusRequest;

/// Marker for [`msg::GetTaskStatusResponse`]（v0.3 B4 `GetTaskStatus` RR reply）。
#[derive(Debug, Clone, Copy, Default)]
pub struct GetTaskStatusResponse;

/// Marker for [`msg::CancelTaskRequest`]（v0.3 B4 `CancelTask` RR payload）。
#[derive(Debug, Clone, Copy, Default)]
pub struct CancelTaskRequest;

/// Marker for [`msg::CancelTaskResponse`]（v0.3 B4 `CancelTask` RR reply）。
#[derive(Debug, Clone, Copy, Default)]
pub struct CancelTaskResponse;

/// Marker for [`msg::ClearErrorRequest`]（v0.3 B4 `ClearError` RR payload）。
#[derive(Debug, Clone, Copy, Default)]
pub struct ClearErrorRequest;

/// Marker for [`msg::ClearErrorResponse`]（v0.3 B4 `ClearError` RR reply）。
#[derive(Debug, Clone, Copy, Default)]
pub struct ClearErrorResponse;

/// Marker for [`msg::ActionEventEnvelope`]（v0.3 B4 `StreamTaskEvents`
/// per-ActionId pubsub topic payload）。
#[derive(Debug, Clone, Copy, Default)]
pub struct ActionEventEnvelope;

/// Marker for [`msg::ScriptedPayload`]（v0.4 候选 1 `ScriptedBackend`
/// `InvokeTask` payload）。
#[derive(Debug, Clone, Copy, Default)]
pub struct ScriptedPayload;

// ---------------------------------------------------------------------------
// BodyMarker 实现
// ---------------------------------------------------------------------------

impl BodyMarker for JointLayout {
    const SCHEMA_ID: u32 = schema_id::SCHEMA_JOINT_LAYOUT;
    const SCHEMA_NAME: &'static str = "robot.msg.JointLayout";
    const BLOB_EMBEDDED: bool = false;
    type Root<'a> = msg::JointLayout<'a>;
}

impl BodyMarker for JointState {
    const SCHEMA_ID: u32 = schema_id::SCHEMA_JOINT_STATE;
    const SCHEMA_NAME: &'static str = "robot.msg.JointState";
    const BLOB_EMBEDDED: bool = false;
    type Root<'a> = msg::JointState<'a>;
}

impl BodyMarker for JointCommand {
    const SCHEMA_ID: u32 = schema_id::SCHEMA_JOINT_COMMAND;
    const SCHEMA_NAME: &'static str = "robot.msg.JointCommand";
    const BLOB_EMBEDDED: bool = false;
    type Root<'a> = msg::JointCommand<'a>;
}

impl BodyMarker for Twist {
    const SCHEMA_ID: u32 = schema_id::SCHEMA_TWIST;
    const SCHEMA_NAME: &'static str = "robot.msg.Twist";
    const BLOB_EMBEDDED: bool = false;
    type Root<'a> = msg::Twist<'a>;
}

impl BodyMarker for ImageHeader {
    const SCHEMA_ID: u32 = schema_id::SCHEMA_IMAGE_HEADER;
    const SCHEMA_NAME: &'static str = "robot.msg.ImageHeader";
    const BLOB_EMBEDDED: bool = true;
    type Root<'a> = msg::ImageHeader<'a>;

    fn read_blob_desc(root: &Self::Root<'_>) -> Option<BlobDescInfo> {
        blob_desc_from(root.blob())
    }
}

impl BodyMarker for PointCloudHeader {
    const SCHEMA_ID: u32 = schema_id::SCHEMA_POINTCLOUD_HEADER;
    const SCHEMA_NAME: &'static str = "robot.msg.PointCloudHeader";
    const BLOB_EMBEDDED: bool = true;
    type Root<'a> = msg::PointCloudHeader<'a>;

    fn read_blob_desc(root: &Self::Root<'_>) -> Option<BlobDescInfo> {
        blob_desc_from(root.blob())
    }
}

impl BodyMarker for TensorHeader {
    const SCHEMA_ID: u32 = schema_id::SCHEMA_TENSOR_HEADER;
    const SCHEMA_NAME: &'static str = "robot.msg.TensorHeader";
    const BLOB_EMBEDDED: bool = true;
    type Root<'a> = msg::TensorHeader<'a>;

    fn read_blob_desc(root: &Self::Root<'_>) -> Option<BlobDescInfo> {
        blob_desc_from(root.blob())
    }
}

impl BodyMarker for EStopEvent {
    const SCHEMA_ID: u32 = schema_id::SCHEMA_ESTOP_EVENT;
    const SCHEMA_NAME: &'static str = "robot.msg.EStopEvent";
    const BLOB_EMBEDDED: bool = false;
    type Root<'a> = msg::EStopEvent<'a>;
}

impl BodyMarker for Heartbeat {
    const SCHEMA_ID: u32 = schema_id::SCHEMA_HEARTBEAT;
    const SCHEMA_NAME: &'static str = "robot.msg.Heartbeat";
    const BLOB_EMBEDDED: bool = false;
    type Root<'a> = msg::Heartbeat<'a>;
}

impl BodyMarker for ClearEStopRequest {
    const SCHEMA_ID: u32 = schema_id::SCHEMA_CLEAR_ESTOP_REQUEST;
    const SCHEMA_NAME: &'static str = "robot.msg.ClearEStopRequest";
    const BLOB_EMBEDDED: bool = false;
    type Root<'a> = msg::ClearEStopRequest<'a>;
}

impl BodyMarker for ClearEStopResponse {
    const SCHEMA_ID: u32 = schema_id::SCHEMA_CLEAR_ESTOP_RESPONSE;
    const SCHEMA_NAME: &'static str = "robot.msg.ClearEStopResponse";
    const BLOB_EMBEDDED: bool = false;
    type Root<'a> = msg::ClearEStopResponse<'a>;
}

impl BodyMarker for InvokeTaskRequest {
    const SCHEMA_ID: u32 = schema_id::SCHEMA_INVOKE_TASK_REQUEST;
    const SCHEMA_NAME: &'static str = "robot.msg.InvokeTaskRequest";
    const BLOB_EMBEDDED: bool = false;
    type Root<'a> = msg::InvokeTaskRequest<'a>;
}

impl BodyMarker for InvokeTaskResponse {
    const SCHEMA_ID: u32 = schema_id::SCHEMA_INVOKE_TASK_RESPONSE;
    const SCHEMA_NAME: &'static str = "robot.msg.InvokeTaskResponse";
    const BLOB_EMBEDDED: bool = false;
    type Root<'a> = msg::InvokeTaskResponse<'a>;
}

impl BodyMarker for GetTaskStatusRequest {
    const SCHEMA_ID: u32 = schema_id::SCHEMA_GET_TASK_STATUS_REQUEST;
    const SCHEMA_NAME: &'static str = "robot.msg.GetTaskStatusRequest";
    const BLOB_EMBEDDED: bool = false;
    type Root<'a> = msg::GetTaskStatusRequest<'a>;
}

impl BodyMarker for GetTaskStatusResponse {
    const SCHEMA_ID: u32 = schema_id::SCHEMA_GET_TASK_STATUS_RESPONSE;
    const SCHEMA_NAME: &'static str = "robot.msg.GetTaskStatusResponse";
    const BLOB_EMBEDDED: bool = false;
    type Root<'a> = msg::GetTaskStatusResponse<'a>;
}

impl BodyMarker for CancelTaskRequest {
    const SCHEMA_ID: u32 = schema_id::SCHEMA_CANCEL_TASK_REQUEST;
    const SCHEMA_NAME: &'static str = "robot.msg.CancelTaskRequest";
    const BLOB_EMBEDDED: bool = false;
    type Root<'a> = msg::CancelTaskRequest<'a>;
}

impl BodyMarker for CancelTaskResponse {
    const SCHEMA_ID: u32 = schema_id::SCHEMA_CANCEL_TASK_RESPONSE;
    const SCHEMA_NAME: &'static str = "robot.msg.CancelTaskResponse";
    const BLOB_EMBEDDED: bool = false;
    type Root<'a> = msg::CancelTaskResponse<'a>;
}

impl BodyMarker for ClearErrorRequest {
    const SCHEMA_ID: u32 = schema_id::SCHEMA_CLEAR_ERROR_REQUEST;
    const SCHEMA_NAME: &'static str = "robot.msg.ClearErrorRequest";
    const BLOB_EMBEDDED: bool = false;
    type Root<'a> = msg::ClearErrorRequest<'a>;
}

impl BodyMarker for ClearErrorResponse {
    const SCHEMA_ID: u32 = schema_id::SCHEMA_CLEAR_ERROR_RESPONSE;
    const SCHEMA_NAME: &'static str = "robot.msg.ClearErrorResponse";
    const BLOB_EMBEDDED: bool = false;
    type Root<'a> = msg::ClearErrorResponse<'a>;
}

impl BodyMarker for ActionEventEnvelope {
    const SCHEMA_ID: u32 = schema_id::SCHEMA_ACTION_EVENT_ENVELOPE;
    const SCHEMA_NAME: &'static str = "robot.msg.ActionEventEnvelope";
    const BLOB_EMBEDDED: bool = false;
    type Root<'a> = msg::ActionEventEnvelope<'a>;
}

impl BodyMarker for ScriptedPayload {
    const SCHEMA_ID: u32 = schema_id::SCHEMA_SCRIPTED_PAYLOAD;
    const SCHEMA_NAME: &'static str = "robot.msg.ScriptedPayload";
    const BLOB_EMBEDDED: bool = false;
    type Root<'a> = msg::ScriptedPayload<'a>;
}

// ---------------------------------------------------------------------------
// 辅助
// ---------------------------------------------------------------------------

fn blob_desc_from(bd: Option<msg::BlobDesc<'_>>) -> Option<BlobDescInfo> {
    let bd = bd?;
    Some(BlobDescInfo {
        alignment: bd.alignment(),
        size: bd.size(),
        offset: bd.offset(),
        dtype_raw: bd.dtype().0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_id_constants_match_registry() {
        assert_eq!(JointLayout::SCHEMA_ID, schema_id::SCHEMA_JOINT_LAYOUT);
        assert_eq!(JointState::SCHEMA_ID, schema_id::SCHEMA_JOINT_STATE);
        assert_eq!(JointCommand::SCHEMA_ID, schema_id::SCHEMA_JOINT_COMMAND);
        assert_eq!(Twist::SCHEMA_ID, schema_id::SCHEMA_TWIST);
        assert_eq!(ImageHeader::SCHEMA_ID, schema_id::SCHEMA_IMAGE_HEADER);
        assert_eq!(
            PointCloudHeader::SCHEMA_ID,
            schema_id::SCHEMA_POINTCLOUD_HEADER
        );
        assert_eq!(TensorHeader::SCHEMA_ID, schema_id::SCHEMA_TENSOR_HEADER);
        assert_eq!(EStopEvent::SCHEMA_ID, schema_id::SCHEMA_ESTOP_EVENT);
        assert_eq!(Heartbeat::SCHEMA_ID, schema_id::SCHEMA_HEARTBEAT);
    }

    // 直接用 `const { ... }` 在编译期断言；clippy::assertions_on_constants
    // 就不再吵，并且"错了连编译都过不了"比运行时 assert 更强。
    const _BLOB_EMBEDDED_FLAGS: () = {
        assert!(!JointLayout::BLOB_EMBEDDED);
        assert!(!JointState::BLOB_EMBEDDED);
        assert!(!JointCommand::BLOB_EMBEDDED);
        assert!(!Twist::BLOB_EMBEDDED);
        assert!(ImageHeader::BLOB_EMBEDDED);
        assert!(PointCloudHeader::BLOB_EMBEDDED);
        assert!(TensorHeader::BLOB_EMBEDDED);
        assert!(!EStopEvent::BLOB_EMBEDDED);
        assert!(!Heartbeat::BLOB_EMBEDDED);
    };

    #[test]
    fn schema_names_match_registry_entries() {
        use crate::schema_id::lookup_name;
        assert_eq!(
            lookup_name(JointState::SCHEMA_ID),
            Some(JointState::SCHEMA_NAME)
        );
        assert_eq!(
            lookup_name(ImageHeader::SCHEMA_ID),
            Some(ImageHeader::SCHEMA_NAME)
        );
        assert_eq!(
            lookup_name(Heartbeat::SCHEMA_ID),
            Some(Heartbeat::SCHEMA_NAME)
        );
    }
}
