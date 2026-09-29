//! `robot-rpc::state_convert` —— v0.5 候选 B：fbs `JointState` → proto
//! `RobotState` 单向转换器 + 4 个 S0 enum 镜像转换。
//!
//! ## 角色
//!
//! `GetState` / `StreamState` 的权威数据源是 S0 发布的 fbs `JointState`
//! （topic `/<robot_id>/s0/<hardware_id>/state`）。gateway 订阅后在进程内把
//! fbs 表转成 proto `RobotState`（wire reply）。本模块是**纯函数**层：无 I/O、
//! 无 iceoryx2、无 tokio；(2/4) gateway 常驻订阅池 + `GetState` handler 调用它。
//!
//! ## 同名 enum 别名纪律（reviewer Plan round 1 阻断 PB1）
//!
//! proto 4 enum 与 fbs 4 enum **同名**（都叫 `S0Mode` 等）。本模块 import
//! 一律加后缀别名 `*Fb`（来自 [`robot_msg::msg`]）/ `*Proto`（来自
//! [`crate::proto`]），**禁裸用同名**。转换 fn 一律 `s0_*_fb_to_proto`。
//!
//! ## 防漂移锁（PE1）
//!
//! flatbuffers Rust codegen 把 enum 生成为 newtype struct
//! `pub struct S0Mode(pub u8)`（**非** Rust 真 enum）→ `match` 必带 `_` arm，
//! 加 fbs variant **不**会编译失败。故防漂移靠 [`tests`] 里的 **variant-count
//! lock**（`*Fb::ENUM_VALUES.len()` 断言）+ 逐 variant 映射测，沿 robot-s1
//! `ActionKind` count-lock 先例。

use robot_msg::msg::{
    JointState, S0ControlLink as S0ControlLinkFb, S0Mode as S0ModeFb,
    S0OutputMode as S0OutputModeFb, S0SafetyState as S0SafetyStateFb,
};

use crate::proto::{
    RobotState, S0ControlLink as S0ControlLinkProto, S0Mode as S0ModeProto,
    S0OutputMode as S0OutputModeProto, S0SafetyState as S0SafetyStateProto,
};

/// fbs `S0Mode` → proto `S0Mode`。`Unknown` 与未来未知 variant → `Unspecified`。
#[allow(clippy::match_same_arms)]
#[must_use]
pub fn s0_mode_fb_to_proto(fb: S0ModeFb) -> S0ModeProto {
    match fb {
        S0ModeFb::Unknown => S0ModeProto::Unspecified,
        S0ModeFb::Safe => S0ModeProto::Safe,
        S0ModeFb::Ready => S0ModeProto::Ready,
        S0ModeFb::Enabled => S0ModeProto::Enabled,
        S0ModeFb::Fault => S0ModeProto::Fault,
        S0ModeFb::Stopping => S0ModeProto::Stopping,
        // PE1：fbs newtype struct，加 variant 不编译失败 → `_` 前向兼容。
        _ => S0ModeProto::Unspecified,
    }
}

/// fbs `S0ControlLink` → proto `S0ControlLink`。
#[allow(clippy::match_same_arms)]
#[must_use]
pub fn s0_control_link_fb_to_proto(fb: S0ControlLinkFb) -> S0ControlLinkProto {
    match fb {
        S0ControlLinkFb::Unknown => S0ControlLinkProto::Unspecified,
        S0ControlLinkFb::Ok => S0ControlLinkProto::Ok,
        S0ControlLinkFb::Lost => S0ControlLinkProto::Lost,
        _ => S0ControlLinkProto::Unspecified,
    }
}

/// fbs `S0OutputMode` → proto `S0OutputMode`。
#[allow(clippy::match_same_arms)]
#[must_use]
pub fn s0_output_mode_fb_to_proto(fb: S0OutputModeFb) -> S0OutputModeProto {
    match fb {
        S0OutputModeFb::Unknown => S0OutputModeProto::Unspecified,
        S0OutputModeFb::Active => S0OutputModeProto::Active,
        S0OutputModeFb::Hold => S0OutputModeProto::Hold,
        S0OutputModeFb::Zero => S0OutputModeProto::Zero,
        S0OutputModeFb::Stop => S0OutputModeProto::Stop,
        _ => S0OutputModeProto::Unspecified,
    }
}

/// fbs `S0SafetyState` → proto `S0SafetyState`。`TorqueUnknown` 与未来未知
/// variant → `TorqueUnknown`（proto 零值，等价 `*_UNSPECIFIED` 槽位）。
#[allow(clippy::match_same_arms)]
#[must_use]
pub fn s0_safety_state_fb_to_proto(fb: S0SafetyStateFb) -> S0SafetyStateProto {
    match fb {
        S0SafetyStateFb::TorqueUnknown => S0SafetyStateProto::TorqueUnknown,
        S0SafetyStateFb::TorqueOn => S0SafetyStateProto::TorqueOn,
        S0SafetyStateFb::StopCommanded => S0SafetyStateProto::StopCommanded,
        S0SafetyStateFb::TorqueOffConfirmed => S0SafetyStateProto::TorqueOffConfirmed,
        S0SafetyStateFb::StopFailed => S0SafetyStateProto::StopFailed,
        S0SafetyStateFb::AdapterUncertain => S0SafetyStateProto::AdapterUncertain,
        _ => S0SafetyStateProto::TorqueUnknown,
    }
}

/// fbs `JointState` → proto `RobotState`。
///
/// `robot_id` / `hardware_id` 由调用方（gateway 订阅池）注入——fbs `JointState`
/// 自身不带这两者（它们编码在 topic `/<robot_id>/s0/<hardware_id>/state` 里）。
///
/// `stamp_ns` 透传 S0 published 值（CLOCK_MONOTONIC_RAW，**非** gateway 自生
/// clock，AC-7）。`fault_code` 透传 `s0_fault_code` 的数值（uint32，**不**
/// enum 化，D6）。
#[must_use]
pub fn joint_state_fb_to_robot_state(
    fb: JointState<'_>,
    robot_id: &str,
    hardware_id: &str,
) -> RobotState {
    let position_rad = fb
        .position_rad()
        .map_or_else(Vec::new, |v| v.iter().collect::<Vec<f64>>());
    RobotState {
        robot_id: robot_id.to_string(),
        hardware_id: hardware_id.to_string(),
        layout_id: fb.layout_id(),
        stamp_ns: fb.stamp_ns(),
        position_rad,
        mode: s0_mode_fb_to_proto(fb.s0_mode()).into(),
        control_link: s0_control_link_fb_to_proto(fb.s0_control_link()).into(),
        output_mode: s0_output_mode_fb_to_proto(fb.s0_output_mode()).into(),
        safety_state: s0_safety_state_fb_to_proto(fb.s0_safety_state()).into(),
        // D6：fault_code 走 uint32 数值透传，不镜像 S0FaultCode enum。
        fault_code: u32::from(fb.s0_fault_code().0),
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
#[allow(clippy::float_cmp)] // position_rad 透传的是精确字面值，float_cmp 是误报
mod tests {
    use super::*;
    use flatbuffers::FlatBufferBuilder;
    use robot_msg::msg::{JointState, JointStateArgs, S0FaultCode as S0FaultCodeFb};

    #[test]
    fn s0_mode_all_variants() {
        assert_eq!(
            s0_mode_fb_to_proto(S0ModeFb::Unknown),
            S0ModeProto::Unspecified
        );
        assert_eq!(s0_mode_fb_to_proto(S0ModeFb::Safe), S0ModeProto::Safe);
        assert_eq!(s0_mode_fb_to_proto(S0ModeFb::Ready), S0ModeProto::Ready);
        assert_eq!(s0_mode_fb_to_proto(S0ModeFb::Enabled), S0ModeProto::Enabled);
        assert_eq!(s0_mode_fb_to_proto(S0ModeFb::Fault), S0ModeProto::Fault);
        assert_eq!(
            s0_mode_fb_to_proto(S0ModeFb::Stopping),
            S0ModeProto::Stopping
        );
    }

    #[test]
    fn s0_control_link_all_variants() {
        assert_eq!(
            s0_control_link_fb_to_proto(S0ControlLinkFb::Unknown),
            S0ControlLinkProto::Unspecified
        );
        assert_eq!(
            s0_control_link_fb_to_proto(S0ControlLinkFb::Ok),
            S0ControlLinkProto::Ok
        );
        assert_eq!(
            s0_control_link_fb_to_proto(S0ControlLinkFb::Lost),
            S0ControlLinkProto::Lost
        );
    }

    #[test]
    fn s0_output_mode_all_variants() {
        assert_eq!(
            s0_output_mode_fb_to_proto(S0OutputModeFb::Unknown),
            S0OutputModeProto::Unspecified
        );
        assert_eq!(
            s0_output_mode_fb_to_proto(S0OutputModeFb::Active),
            S0OutputModeProto::Active
        );
        assert_eq!(
            s0_output_mode_fb_to_proto(S0OutputModeFb::Hold),
            S0OutputModeProto::Hold
        );
        assert_eq!(
            s0_output_mode_fb_to_proto(S0OutputModeFb::Zero),
            S0OutputModeProto::Zero
        );
        assert_eq!(
            s0_output_mode_fb_to_proto(S0OutputModeFb::Stop),
            S0OutputModeProto::Stop
        );
    }

    #[test]
    fn s0_safety_state_all_variants() {
        assert_eq!(
            s0_safety_state_fb_to_proto(S0SafetyStateFb::TorqueUnknown),
            S0SafetyStateProto::TorqueUnknown
        );
        assert_eq!(
            s0_safety_state_fb_to_proto(S0SafetyStateFb::TorqueOn),
            S0SafetyStateProto::TorqueOn
        );
        assert_eq!(
            s0_safety_state_fb_to_proto(S0SafetyStateFb::StopCommanded),
            S0SafetyStateProto::StopCommanded
        );
        assert_eq!(
            s0_safety_state_fb_to_proto(S0SafetyStateFb::TorqueOffConfirmed),
            S0SafetyStateProto::TorqueOffConfirmed
        );
        assert_eq!(
            s0_safety_state_fb_to_proto(S0SafetyStateFb::StopFailed),
            S0SafetyStateProto::StopFailed
        );
        assert_eq!(
            s0_safety_state_fb_to_proto(S0SafetyStateFb::AdapterUncertain),
            S0SafetyStateProto::AdapterUncertain
        );
    }

    /// PE1 防漂移锁：fbs enum 加 variant → 这些断言挂 → 强制同步更新对应
    /// `s0_*_fb_to_proto` 转换器 + proto enum + 本断言（沿 ActionKind count-lock）。
    #[test]
    fn fbs_enum_variant_count_lock() {
        assert_eq!(
            S0ModeFb::ENUM_VALUES.len(),
            6,
            "fbs S0Mode 变更 → 同步转换器"
        );
        assert_eq!(
            S0ControlLinkFb::ENUM_VALUES.len(),
            3,
            "fbs S0ControlLink 变更 → 同步转换器"
        );
        assert_eq!(
            S0OutputModeFb::ENUM_VALUES.len(),
            5,
            "fbs S0OutputMode 变更 → 同步转换器"
        );
        assert_eq!(
            S0SafetyStateFb::ENUM_VALUES.len(),
            6,
            "fbs S0SafetyState 变更 → 同步转换器"
        );
    }

    #[test]
    fn joint_state_to_robot_state_full_passthrough() {
        let mut fbb = FlatBufferBuilder::new();
        let pos = fbb.create_vector(&[1.0_f64, -2.5, 3.25]);
        let js = JointState::create(
            &mut fbb,
            &JointStateArgs {
                layout_id: 42,
                position_rad: Some(pos),
                stamp_ns: 123_456_789,
                s0_mode: S0ModeFb::Enabled,
                s0_control_link: S0ControlLinkFb::Ok,
                s0_output_mode: S0OutputModeFb::Active,
                s0_safety_state: S0SafetyStateFb::TorqueOn,
                s0_fault_code: S0FaultCodeFb::AdapterErr,
                ..Default::default()
            },
        );
        fbb.finish(js, None);
        let fb = flatbuffers::root::<JointState>(fbb.finished_data()).unwrap();

        let rs = joint_state_fb_to_robot_state(fb, "robotX", "armY");

        assert_eq!(rs.robot_id, "robotX");
        assert_eq!(rs.hardware_id, "armY");
        assert_eq!(rs.layout_id, 42);
        assert_eq!(rs.stamp_ns, 123_456_789);
        assert_eq!(rs.position_rad, vec![1.0, -2.5, 3.25]);
        // 逐字段验证 enum 没有错位（mode≠output_mode 等）。
        assert_eq!(rs.mode, S0ModeProto::Enabled as i32);
        assert_eq!(rs.control_link, S0ControlLinkProto::Ok as i32);
        assert_eq!(rs.output_mode, S0OutputModeProto::Active as i32);
        assert_eq!(rs.safety_state, S0SafetyStateProto::TorqueOn as i32);
        // fault_code 数值透传：S0FaultCode::AdapterErr = 1。
        assert_eq!(rs.fault_code, 1);
    }

    #[test]
    fn joint_state_defaults_map_to_unspecified() {
        // build_joint_state 不写 s0_* → fbs 默认 Unknown/TorqueUnknown。
        let body = robot_msg::build_helpers::build_joint_state(7, 99, &[], &[], &[], "");
        let fb = flatbuffers::root::<JointState>(&body).unwrap();
        let rs = joint_state_fb_to_robot_state(fb, "r", "h");

        assert_eq!(rs.layout_id, 7);
        assert_eq!(rs.stamp_ns, 99);
        assert!(
            rs.position_rad.is_empty(),
            "expected empty positions, got {:?}",
            rs.position_rad
        );
        assert_eq!(rs.mode, S0ModeProto::Unspecified as i32);
        assert_eq!(rs.control_link, S0ControlLinkProto::Unspecified as i32);
        assert_eq!(rs.output_mode, S0OutputModeProto::Unspecified as i32);
        assert_eq!(rs.safety_state, S0SafetyStateProto::TorqueUnknown as i32);
        assert_eq!(rs.fault_code, 0);
    }
}
