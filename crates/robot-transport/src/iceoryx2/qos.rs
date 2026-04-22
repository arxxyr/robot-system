//! `QoS` 映射：`docs/00_design.md` §2.4 九类 topic → iceoryx2 `publish_subscribe`
//! builder 选项（决策 ③）。
//!
//! 具体数值冻结在 `docs/CHANGELOG.md` 2026-04-22 · Step 5 · "`QoS` 映射"小节。
//!
//! 本模块只给类型 + `const fn profile_of`，不直接调 iceoryx2 builder。实际
//! 的 "apply profile to builder" 由后续 `publisher.rs` / `subscriber.rs`
//! 在创建 service 时调用，以避免本模块依赖 iceoryx2 builder 的泛型签名
//! （`Payload: ?Sized` + `UserHeader: Debug + ZeroCopySend` + `Service` 三
//! 元组，给纯类型层套进来会把测试复杂化）。

/// §2.4 的九类 topic + policy stream（按决策 ③ 追加）。
///
/// 归类原则：按 `QoS` 行为相同聚，同一 topic namespace 下可能跨类（例如
/// `/<r>/events/*` 既可能承载 `TaskEvent`，也可能承载 `SafetyEvent`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TopicClass {
    /// `/<r>/s0/<hw>/command`：latest-only，过时由 S0 `on_timeout` 兜底。
    S0Command,
    /// `/<r>/s0/<hw>/state`：drop old OK。
    S0State,
    /// `/<r>/s1/trajectory/<task>`：丢弃必报警，history 深。
    S1Trajectory,
    /// `/<r>/s1/cmd_vel`：latest-only。
    S1CmdVel,
    /// `/<r>/sensors/**`：drop old OK。
    Sensor,
    /// `/<r>/events/estop` / 各类 error：不丢，满则升 fault。
    SafetyEvent,
    /// `/<r>/s1/task_status`：丢弃必可观测。
    TaskEvent,
    /// `/<r>/policy/<name>/observation` / `action_chunk`：S1 永不等 policy。
    PolicyStream,
}

/// 具体 iceoryx2 builder 参数集合。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QosProfile {
    /// `history_size`：publisher 保留多少已发布样本供新订阅者回放。
    pub history_size: usize,
    /// `subscriber_max_buffer_size`：订阅端缓冲深度（未取出 sample 上限）。
    pub subscriber_max_buffer_size: usize,
    /// `max_publishers`：同 service 上 publisher 数硬上限。
    pub max_publishers: usize,
    /// `max_subscribers`：同 service 上 subscriber 数硬上限。
    pub max_subscribers: usize,
    /// `enable_safe_overflow`：缓冲满时是否允许 publisher 覆盖最老样本。
    /// - `true` → drop-old（"latest" 语义，sensor / state）
    /// - `false` → fail-fast（"必须到达" 语义，trajectory / safety）
    pub safe_overflow: bool,
}

impl QosProfile {
    /// 保守默认（供测试 / fallback 使用）：单 pub 单 sub，history=1，
    /// 不允许 overflow（最严格）。
    #[must_use]
    pub const fn conservative() -> Self {
        Self {
            history_size: 1,
            subscriber_max_buffer_size: 1,
            max_publishers: 1,
            max_subscribers: 1,
            safe_overflow: false,
        }
    }
}

/// 按 [`TopicClass`] 查 [`QosProfile`]。
///
/// 数值来源：`docs/CHANGELOG.md` 2026-04-22 · Step 5 · "`QoS` 映射表"。
#[inline]
#[must_use]
pub const fn profile_of(class: TopicClass) -> QosProfile {
    match class {
        TopicClass::S0Command => QosProfile {
            history_size: 1,
            subscriber_max_buffer_size: 1,
            max_publishers: 1,
            max_subscribers: 1,
            safe_overflow: false,
        },
        TopicClass::S0State => QosProfile {
            history_size: 2,
            subscriber_max_buffer_size: 2,
            max_publishers: 1,
            // N 订阅者：viz + recorder + s1 coordinator + gateway 客户端等
            max_subscribers: 8,
            safe_overflow: true,
        },
        TopicClass::S1Trajectory => QosProfile {
            history_size: 8,
            subscriber_max_buffer_size: 16,
            max_publishers: 1,
            max_subscribers: 8,
            safe_overflow: false,
        },
        TopicClass::S1CmdVel => QosProfile {
            history_size: 1,
            subscriber_max_buffer_size: 1,
            max_publishers: 1,
            max_subscribers: 1,
            safe_overflow: true,
        },
        TopicClass::Sensor => QosProfile {
            history_size: 2,
            subscriber_max_buffer_size: 4,
            max_publishers: 1,
            max_subscribers: 8,
            safe_overflow: true,
        },
        TopicClass::SafetyEvent => QosProfile {
            history_size: 32,
            subscriber_max_buffer_size: 64,
            // N publisher：s0 + s1 + external estop + manual trigger
            max_publishers: 4,
            max_subscribers: 8,
            safe_overflow: false,
        },
        TopicClass::TaskEvent => QosProfile {
            history_size: 64,
            subscriber_max_buffer_size: 64,
            max_publishers: 1,
            max_subscribers: 8,
            safe_overflow: false,
        },
        TopicClass::PolicyStream => QosProfile {
            history_size: 1,
            subscriber_max_buffer_size: 2,
            max_publishers: 1,
            max_subscribers: 1,
            safe_overflow: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conservative_default_is_most_strict() {
        let c = QosProfile::conservative();
        assert_eq!(c.history_size, 1);
        assert_eq!(c.subscriber_max_buffer_size, 1);
        assert_eq!(c.max_publishers, 1);
        assert_eq!(c.max_subscribers, 1);
        assert!(!c.safe_overflow);
    }

    #[test]
    fn all_classes_have_positive_capacity() {
        for class in [
            TopicClass::S0Command,
            TopicClass::S0State,
            TopicClass::S1Trajectory,
            TopicClass::S1CmdVel,
            TopicClass::Sensor,
            TopicClass::SafetyEvent,
            TopicClass::TaskEvent,
            TopicClass::PolicyStream,
        ] {
            let p = profile_of(class);
            assert!(p.history_size >= 1, "{class:?} history_size = 0");
            assert!(
                p.subscriber_max_buffer_size >= 1,
                "{class:?} subscriber_max_buffer_size = 0"
            );
            assert!(p.max_publishers >= 1, "{class:?} max_publishers = 0");
            assert!(p.max_subscribers >= 1, "{class:?} max_subscribers = 0");
        }
    }

    #[test]
    fn safe_overflow_semantics() {
        // "latest-wins" 语义：state / cmd_vel / sensor 允许覆盖
        for class in [
            TopicClass::S0State,
            TopicClass::S1CmdVel,
            TopicClass::Sensor,
        ] {
            assert!(
                profile_of(class).safe_overflow,
                "{class:?} 预期 safe_overflow=true"
            );
        }
        // "必须到达" 语义：command / trajectory / safety / task / policy 不允许丢
        for class in [
            TopicClass::S0Command,
            TopicClass::S1Trajectory,
            TopicClass::SafetyEvent,
            TopicClass::TaskEvent,
            TopicClass::PolicyStream,
        ] {
            assert!(
                !profile_of(class).safe_overflow,
                "{class:?} 预期 safe_overflow=false"
            );
        }
    }

    #[test]
    fn s0_command_is_latest_only() {
        let p = profile_of(TopicClass::S0Command);
        assert_eq!(p.history_size, 1);
        assert_eq!(p.subscriber_max_buffer_size, 1);
        assert!(!p.safe_overflow); // 超时由 S0 on_timeout 兜底，不吞
    }

    #[test]
    fn safety_event_has_high_headroom() {
        let p = profile_of(TopicClass::SafetyEvent);
        assert!(p.history_size >= 32);
        assert!(p.subscriber_max_buffer_size >= 64);
        assert!(p.max_publishers >= 2); // 至少允许 s0 + s1 同时报
    }

    #[test]
    fn trajectory_history_beats_subscribe_buffer_is_half_or_less() {
        // trajectory 需要新订阅者回放多段历史；subscribe buffer 应 >= history
        let p = profile_of(TopicClass::S1Trajectory);
        assert!(p.subscriber_max_buffer_size >= p.history_size);
    }
}
