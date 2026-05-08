//! `events_drain` wire codec 层 —— `ActionEvent` ↔ fbs `ActionEventEnvelope`
//! 编码 helpers + terminal 判定。
//!
//! ## 边界
//!
//! - **不**接 iceoryx2 / **不**起线程 / **不**碰 [`crate::coordinator::Coordinator`]；
//!   纯 [`ActionEvent`] → `Vec<u8>` 转换 + 枚举映射。
//! - 跟 [`super::runtime`] 解耦 —— runtime 的 `run_drain_loop` 只调
//!   [`encode_envelope`] / [`is_terminal`]，不依赖 wire 内部 layout。

#![allow(clippy::doc_markdown)]

use robot_msg::build_helpers::build_action_event_envelope;
use robot_msg::msg::{ActionEventKindFb, ActionFailureKindFb, ActionOutcomeKindFb};

use crate::action::{ActionEvent, ActionFailure, ActionOutcome};

// ============================================================================
// `ActionEvent` → `ActionEventEnvelope` 编码
// ============================================================================

/// 把 lib [`ActionEvent`] 5 个 variant 映射到 fbs `ActionEventEnvelope`
/// 字段集；返完整 wire body bytes（`build_action_event_envelope` 内部走
/// `FlatBufferBuilder`）。
///
/// 时间戳来源：`at_ns` = `event` 本身的 `at_ns` 字段（事件**语义**时刻）。
/// 跟 `FrameHeader.stamp_ns`（wire publish 时刻）区分。
#[must_use]
pub fn encode_envelope(action_id_hex: &str, event: &ActionEvent) -> Vec<u8> {
    match event {
        ActionEvent::Started { at_ns } => build_action_event_envelope(
            action_id_hex,
            *at_ns,
            ActionEventKindFb::Started,
            0.0,
            None,
            None,
        ),
        ActionEvent::Progress { progress, at_ns } => build_action_event_envelope(
            action_id_hex,
            *at_ns,
            ActionEventKindFb::Progress,
            *progress,
            None,
            None,
        ),
        ActionEvent::Completed { outcome, at_ns } => build_action_event_envelope(
            action_id_hex,
            *at_ns,
            ActionEventKindFb::Completed,
            0.0,
            Some(map_outcome(outcome)),
            None,
        ),
        ActionEvent::Failed { failure, at_ns } => build_action_event_envelope(
            action_id_hex,
            *at_ns,
            ActionEventKindFb::Failed,
            0.0,
            None,
            Some(map_failure(failure)),
        ),
        ActionEvent::Cancelled { at_ns } => build_action_event_envelope(
            action_id_hex,
            *at_ns,
            ActionEventKindFb::Cancelled,
            0.0,
            None,
            None,
        ),
    }
}

/// 判定一个 [`ActionEvent`] 是否是终态事件（Completed / Failed / Cancelled）。
/// drain thread 在收到第一个终态事件后 publish + sleep `terminal_grace` +
/// 退出（reviewer 拍板"绝不双发 Completed + Cancelled"）。
#[inline]
#[must_use]
pub fn is_terminal(event: &ActionEvent) -> bool {
    matches!(
        event,
        ActionEvent::Completed { .. } | ActionEvent::Failed { .. } | ActionEvent::Cancelled { .. }
    )
}

#[inline]
fn map_outcome(outcome: &ActionOutcome) -> (ActionOutcomeKindFb, &str) {
    match outcome {
        ActionOutcome::Success => (ActionOutcomeKindFb::Success, ""),
        ActionOutcome::PartialSuccess { warning } => {
            (ActionOutcomeKindFb::PartialSuccess, warning.as_str())
        }
    }
}

#[inline]
fn map_failure(failure: &ActionFailure) -> (ActionFailureKindFb, &str) {
    match failure {
        ActionFailure::DeadlineExceeded => (ActionFailureKindFb::DeadlineExceeded, ""),
        ActionFailure::PreconditionFailed(s) => {
            (ActionFailureKindFb::PreconditionFailed, s.as_str())
        }
        ActionFailure::AdapterError(s) => (ActionFailureKindFb::AdapterError, s.as_str()),
        ActionFailure::SafetyAbort(s) => (ActionFailureKindFb::SafetyAbort, s.as_str()),
        ActionFailure::InternalError(s) => (ActionFailureKindFb::InternalError, s.as_str()),
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use robot_msg::msg::{ActionEventEnvelope as FbEnv, ActionEventKindFb as FbKind};

    fn decode_kind(bytes: &[u8]) -> FbKind {
        let env = flatbuffers::root::<FbEnv>(bytes).expect("decode env");
        env.kind()
    }

    #[test]
    fn encode_started_round_trip() {
        let bytes = encode_envelope("aid-1", &ActionEvent::Started { at_ns: 100 });
        let env = flatbuffers::root::<FbEnv>(&bytes).unwrap();
        assert_eq!(env.kind(), FbKind::Started);
        assert_eq!(env.action_id().unwrap(), "aid-1");
        assert_eq!(env.at_ns(), 100);
        assert!((env.progress() - 0.0).abs() < f32::EPSILON);
        assert!(env.outcome().is_none());
        assert!(env.failure().is_none());
    }

    #[test]
    fn encode_progress_round_trip() {
        let bytes = encode_envelope(
            "aid-2",
            &ActionEvent::Progress {
                progress: 0.42,
                at_ns: 200,
            },
        );
        let env = flatbuffers::root::<FbEnv>(&bytes).unwrap();
        assert_eq!(env.kind(), FbKind::Progress);
        assert!((env.progress() - 0.42).abs() < f32::EPSILON);
    }

    #[test]
    fn encode_completed_partial_success_round_trip() {
        let bytes = encode_envelope(
            "aid-3",
            &ActionEvent::Completed {
                outcome: ActionOutcome::PartialSuccess {
                    warning: "limit".into(),
                },
                at_ns: 300,
            },
        );
        let env = flatbuffers::root::<FbEnv>(&bytes).unwrap();
        assert_eq!(env.kind(), FbKind::Completed);
        let outcome = env.outcome().expect("outcome present");
        assert_eq!(outcome.kind(), ActionOutcomeKindFb::PartialSuccess);
        assert_eq!(outcome.warning().unwrap(), "limit");
    }

    #[test]
    fn encode_failed_safety_abort_round_trip() {
        let bytes = encode_envelope(
            "aid-4",
            &ActionEvent::Failed {
                failure: ActionFailure::SafetyAbort("e-stop".into()),
                at_ns: 400,
            },
        );
        let env = flatbuffers::root::<FbEnv>(&bytes).unwrap();
        assert_eq!(env.kind(), FbKind::Failed);
        let failure = env.failure().expect("failure present");
        assert_eq!(failure.kind(), ActionFailureKindFb::SafetyAbort);
        assert_eq!(failure.reason().unwrap(), "e-stop");
    }

    #[test]
    fn encode_cancelled_round_trip() {
        let bytes = encode_envelope("aid-5", &ActionEvent::Cancelled { at_ns: 500 });
        assert_eq!(decode_kind(&bytes), FbKind::Cancelled);
    }

    #[test]
    fn is_terminal_for_each_variant() {
        assert!(!is_terminal(&ActionEvent::Started { at_ns: 0 }));
        assert!(!is_terminal(&ActionEvent::Progress {
            progress: 0.5,
            at_ns: 0
        }));
        assert!(is_terminal(&ActionEvent::Completed {
            outcome: ActionOutcome::Success,
            at_ns: 0
        }));
        assert!(is_terminal(&ActionEvent::Failed {
            failure: ActionFailure::DeadlineExceeded,
            at_ns: 0
        }));
        assert!(is_terminal(&ActionEvent::Cancelled { at_ns: 0 }));
    }

    /// 全 ActionFailure variant 的 fbs kind 映射锁测（reason 字串透传）。
    #[test]
    fn map_failure_all_variants() {
        let cases: &[(ActionFailure, ActionFailureKindFb, &str)] = &[
            (
                ActionFailure::DeadlineExceeded,
                ActionFailureKindFb::DeadlineExceeded,
                "",
            ),
            (
                ActionFailure::PreconditionFailed("pf".into()),
                ActionFailureKindFb::PreconditionFailed,
                "pf",
            ),
            (
                ActionFailure::AdapterError("ae".into()),
                ActionFailureKindFb::AdapterError,
                "ae",
            ),
            (
                ActionFailure::SafetyAbort("sa".into()),
                ActionFailureKindFb::SafetyAbort,
                "sa",
            ),
            (
                ActionFailure::InternalError("ie".into()),
                ActionFailureKindFb::InternalError,
                "ie",
            ),
        ];
        for (failure, expected_kind, expected_reason) in cases {
            let (kind, reason) = map_failure(failure);
            assert_eq!(kind, *expected_kind, "kind for {failure:?}");
            assert_eq!(reason, *expected_reason, "reason for {failure:?}");
        }
    }
}
