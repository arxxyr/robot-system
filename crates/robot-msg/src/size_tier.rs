//! Iceoryx2 sample 大小分档（`docs/CHANGELOG.md` 2026-04-22 · Step 5 决策 ④）。
//!
//! 每个 `schema_id` 映射到固定的 [`SizeTier`]（硬编码 default），transport 层
//! 据此向 iceoryx2 service builder 声明 `max_sample_size`；TOML 层运行期覆盖
//! 特定 topic 上限（override 机制推迟到 v0.3，本模块只管 default）。
//!
//! **约束**：
//! 1. [`default_tier_of`] 对未登记 schema 返回 `None`，transport 侧 fail-fast。
//! 2. [`tests::every_registered_schema_has_a_tier`] 穷举 [`crate::schema_id::SCHEMA_REGISTRY`]；
//!    新增 schema 忘记加 tier → 测试失败。
//! 3. 所有函数都是 `const fn`，不涉及分配 / 查表开销。

use crate::schema_id::{
    SCHEMA_ACTION_EVENT_ENVELOPE, SCHEMA_CANCEL_TASK_REQUEST, SCHEMA_CANCEL_TASK_RESPONSE,
    SCHEMA_CLEAR_ERROR_REQUEST, SCHEMA_CLEAR_ERROR_RESPONSE, SCHEMA_CLEAR_ESTOP_REQUEST,
    SCHEMA_CLEAR_ESTOP_RESPONSE, SCHEMA_ESTOP_EVENT, SCHEMA_GET_TASK_STATUS_REQUEST,
    SCHEMA_GET_TASK_STATUS_RESPONSE, SCHEMA_HEARTBEAT, SCHEMA_IK_PAYLOAD, SCHEMA_IMAGE_HEADER,
    SCHEMA_INVOKE_TASK_REQUEST, SCHEMA_INVOKE_TASK_RESPONSE, SCHEMA_JOINT_COMMAND,
    SCHEMA_JOINT_LAYOUT, SCHEMA_JOINT_STATE, SCHEMA_POINTCLOUD_HEADER, SCHEMA_SCRIPTED_PAYLOAD,
    SCHEMA_TENSOR_HEADER, SCHEMA_TWIST,
};

/// Iceoryx2 sample 大小档位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SizeTier {
    /// 控制 / 事件 / 心跳 / 关节状态 · 8 KiB。
    Control,
    /// 小 sensor（IMU / FT / 低分辨率点云等） · 1 MiB。
    SensorSmall,
    /// 常规相机 / 大点云 / 轨迹 · 16 MiB。
    SensorLarge,
    /// VLA observation / action chunk / 超大 tensor · 128 MiB。
    Tensor,
}

impl SizeTier {
    /// 档位的 `max_sample_size` 上限（字节）。
    ///
    /// 数值选定见 `docs/CHANGELOG.md` 2026-04-22 · "Size tier" 小节。
    /// 幂次对齐方便 iceoryx2 shm 预算估算 + cache line 友好。
    #[inline]
    #[must_use]
    pub const fn max_bytes(self) -> usize {
        match self {
            Self::Control => 8 * 1024,             // 8 KiB
            Self::SensorSmall => 1024 * 1024,      // 1 MiB
            Self::SensorLarge => 16 * 1024 * 1024, // 16 MiB
            Self::Tensor => 128 * 1024 * 1024,     // 128 MiB
        }
    }
}

/// 按 `schema_id` 查默认 [`SizeTier`]。未登记返回 `None`。
///
/// `None` 的语义：transport 侧拒绝为该 schema 创建 service（fail-fast，禁止
/// 落默认 tier 吞掉 "这是个新 schema，我不知道多大" 的信号）。
///
/// 穷举测试在 [`tests::every_registered_schema_has_a_tier`]；加新 schema
/// 必同步加 match arm，否则 `cargo test -p robot-msg` 失败。
#[inline]
#[must_use]
pub const fn default_tier_of(schema_id: u32) -> Option<SizeTier> {
    match schema_id {
        // control：关节 / 运动 / 事件 / 心跳 / control RPC payload（A1
        // ClearEStop + B4 task control plane 4 RR + 1 pubsub event）
        SCHEMA_JOINT_LAYOUT
        | SCHEMA_JOINT_STATE
        | SCHEMA_JOINT_COMMAND
        | SCHEMA_TWIST
        | SCHEMA_ESTOP_EVENT
        | SCHEMA_HEARTBEAT
        | SCHEMA_CLEAR_ESTOP_REQUEST
        | SCHEMA_CLEAR_ESTOP_RESPONSE
        | SCHEMA_INVOKE_TASK_REQUEST
        | SCHEMA_INVOKE_TASK_RESPONSE
        | SCHEMA_GET_TASK_STATUS_REQUEST
        | SCHEMA_GET_TASK_STATUS_RESPONSE
        | SCHEMA_CANCEL_TASK_REQUEST
        | SCHEMA_CANCEL_TASK_RESPONSE
        | SCHEMA_CLEAR_ERROR_REQUEST
        | SCHEMA_CLEAR_ERROR_RESPONSE
        | SCHEMA_ACTION_EVENT_ENVELOPE
        | SCHEMA_SCRIPTED_PAYLOAD
        | SCHEMA_IK_PAYLOAD => Some(SizeTier::Control),

        // sensor_small：未来 IMU / FT 等小 sensor 在此扩。
        // 目前无注册的 schema 落此档。

        // sensor_large：图像 / 点云 / 通用 tensor（默认走 16 MiB；VLA / 超大
        // tensor 场景由 runtime TOML override 升档到 Tensor，见 CHANGELOG 2026-04-22）。
        SCHEMA_IMAGE_HEADER | SCHEMA_POINTCLOUD_HEADER | SCHEMA_TENSOR_HEADER => {
            Some(SizeTier::SensorLarge)
        }

        // tensor：未来 SCHEMA_POLICY_OBSERVATION / SCHEMA_ACTION_CHUNK 在此扩。
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema_id::SCHEMA_REGISTRY;

    #[test]
    fn max_bytes_are_powers_of_two() {
        for tier in [
            SizeTier::Control,
            SizeTier::SensorSmall,
            SizeTier::SensorLarge,
            SizeTier::Tensor,
        ] {
            let b = tier.max_bytes();
            assert!(
                b > 0 && b.is_power_of_two(),
                "tier {tier:?} = {b} 不是 2 的幂"
            );
        }
    }

    #[test]
    fn max_bytes_monotonic_by_tier() {
        assert!(SizeTier::Control.max_bytes() < SizeTier::SensorSmall.max_bytes());
        assert!(SizeTier::SensorSmall.max_bytes() < SizeTier::SensorLarge.max_bytes());
        assert!(SizeTier::SensorLarge.max_bytes() < SizeTier::Tensor.max_bytes());
    }

    #[test]
    fn max_bytes_exact_values() {
        assert_eq!(SizeTier::Control.max_bytes(), 8 * 1024);
        assert_eq!(SizeTier::SensorSmall.max_bytes(), 1024 * 1024);
        assert_eq!(SizeTier::SensorLarge.max_bytes(), 16 * 1024 * 1024);
        assert_eq!(SizeTier::Tensor.max_bytes(), 128 * 1024 * 1024);
    }

    #[test]
    fn every_registered_schema_has_a_tier() {
        // 核心穷举测试：加新 schema 必须同步在 default_tier_of 里列。
        for (id, name) in SCHEMA_REGISTRY {
            assert!(
                default_tier_of(*id).is_some(),
                "schema {name:?} (id={id:#010x}) 未在 default_tier_of 登记 tier。\
                 新加 schema 必须同步在 crates/robot-msg/src/size_tier.rs 的 match 里加 arm。",
            );
        }
    }

    #[test]
    fn unregistered_schema_id_returns_none() {
        assert_eq!(default_tier_of(0xDEAD_BEEF), None);
        assert_eq!(default_tier_of(0x0000_0000), None);
        assert_eq!(default_tier_of(0x7FFF_FFFF), None);
    }

    #[test]
    fn control_tier_covers_expected_schemas() {
        for id in [
            SCHEMA_JOINT_LAYOUT,
            SCHEMA_JOINT_STATE,
            SCHEMA_JOINT_COMMAND,
            SCHEMA_TWIST,
            SCHEMA_ESTOP_EVENT,
            SCHEMA_HEARTBEAT,
            SCHEMA_CLEAR_ESTOP_REQUEST,
            SCHEMA_CLEAR_ESTOP_RESPONSE,
        ] {
            assert_eq!(
                default_tier_of(id),
                Some(SizeTier::Control),
                "schema {id:#010x} 预期 Control 档",
            );
        }
    }

    #[test]
    fn sensor_large_tier_covers_expected_schemas() {
        for id in [
            SCHEMA_IMAGE_HEADER,
            SCHEMA_POINTCLOUD_HEADER,
            SCHEMA_TENSOR_HEADER,
        ] {
            assert_eq!(
                default_tier_of(id),
                Some(SizeTier::SensorLarge),
                "schema {id:#010x} 预期 SensorLarge 档",
            );
        }
    }
}
