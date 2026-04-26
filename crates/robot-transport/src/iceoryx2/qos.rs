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
    /// `/<r>/s1/cmd_vel`：latest-only **单订阅**（S1 → S0 速度命令，只有 S0 消费）。
    S1CmdVel,
    /// `/<r>/s0/heartbeat` / `/<r>/s1/heartbeat`：心跳广播，多订阅观测路径
    /// （S0 hb 的订阅方至少包括 S0 自身 watchdog + recorder + viz + robotctl
    /// watch + gateway 客户端；S1 hb 早期只有 S0 一个订阅者，但保留扩展余量
    /// 以便 recorder 等工具同步看 S1 心跳）。
    ///
    /// 行为：drop-old（最新心跳即真实存活信号），多订阅 + 大缓冲。
    ///
    /// **回归源（2026-04-26）**：原本所有 heartbeat 站点借用
    /// `S1CmdVel`（`max_subscribers=1`），导致 `robotctl watch` 跟其它
    /// observer 同跑时第二个 subscriber 拿不到 sample，runbook §4-§7 wire
    /// 三方合验路径打折扣（详 `docs/handoffs/2026-04-26.md` §5.1 known-issue
    /// 在本 commit 被否决：不是 iceoryx2 框架 bug，是我们的 `QoS` 分类错误）。
    Heartbeat,
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
        TopicClass::Heartbeat => QosProfile {
            // history=4 给新 attach 的订阅者 ~80 ms (50 Hz × 4) 心跳热身，让
            // recorder / viz 重连后能立刻判活；同时把这条跟 Sensor profile 错开
            // （clippy `match_same_arms` 的实际意图是别让两条策略意外趋同）。
            history_size: 4,
            // 4 帧缓冲足够吸收 watch / recorder 这种 100~250 ms 间隔的 polling
            // 跟 50 Hz publisher 之间的临时落后；safe_overflow=true 让快 publisher
            // 永远能写下去。
            subscriber_max_buffer_size: 4,
            max_publishers: 1,
            // N 订阅者：s0 自身 watchdog（订 S1 hb）/ recorder / viz /
            // robotctl watch / gateway / 备用 = 8 个上限留充分 headroom。
            max_subscribers: 8,
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
            TopicClass::Heartbeat,
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

    /// 锁住 heartbeat / 多观测者类 topic 的"必须支持多 sub"语义 —— 防止
    /// 哪天有人手抖把 `max_subscribers` 改回 1，又复现 2026-04-26 那个
    /// `robotctl watch` 第二个 sub 拿不到 sample 的回归。
    #[test]
    fn observer_classes_support_multi_subscriber() {
        for class in [
            TopicClass::S0State,
            TopicClass::Heartbeat,
            TopicClass::Sensor,
            TopicClass::SafetyEvent,
            TopicClass::TaskEvent,
        ] {
            let p = profile_of(class);
            assert!(
                p.max_subscribers >= 8,
                "{class:?} max_subscribers={} 应 >= 8（多观测者：recorder + viz + cli + ...）",
                p.max_subscribers
            );
        }
    }

    /// Heartbeat 必须 drop-old：50 Hz 心跳，对慢订阅者覆盖最老才是合理语义。
    /// 如果 `safe_overflow=false` 一旦 buffer 满 publisher 直接 fail，会把 S0
    /// 的 50 Hz `pub_thread` 卡住，违反 §0 "RT thread 不能阻塞" 的口风。
    #[test]
    fn heartbeat_is_drop_old() {
        let p = profile_of(TopicClass::Heartbeat);
        assert!(
            p.safe_overflow,
            "Heartbeat 必须 safe_overflow=true（drop-old）"
        );
        assert!(
            p.subscriber_max_buffer_size >= 2,
            "Heartbeat subscriber_max_buffer_size 至少 2，吸收慢 polling"
        );
    }

    #[test]
    fn safe_overflow_semantics() {
        // "latest-wins" 语义：state / cmd_vel / heartbeat / sensor 允许覆盖
        for class in [
            TopicClass::S0State,
            TopicClass::S1CmdVel,
            TopicClass::Heartbeat,
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
