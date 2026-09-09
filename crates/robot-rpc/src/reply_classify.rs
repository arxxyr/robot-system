//! 回复退休证据的纯分类；不操作队列、许可或缓存。

use crate::class_cache::{CachedTerminal, TerminalState, TypedOutcome};
use crate::idempotency::Method;
use robot_msg::msg::SideEffect;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldPresence<T> {
    Present(T),
    Absent,
}
#[derive(Debug)]
pub struct Expected {
    pub method: Method,
    pub request_id: String,
}
#[derive(Debug)]
pub struct ReplyView<'a> {
    pub method: Method,
    pub request_id: &'a str,
    pub retired: FieldPresence<bool>,
    pub effect: SideEffect,
    pub outcome: TypedOutcome,
    pub success: bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Mismatch,
    Pending,
    Abandon,
    Terminal(CachedTerminal),
}

#[must_use]
pub fn classify(expected: &Expected, view: &ReplyView<'_>) -> Verdict {
    if expected.method != view.method || expected.request_id != view.request_id {
        return Verdict::Mismatch;
    }
    if view.retired != FieldPresence::Present(true) {
        return Verdict::Pending;
    }
    let state = match view.effect {
        SideEffect::Unknown => TerminalState::UnknownRetired,
        SideEffect::None if view.success => TerminalState::Committed,
        SideEffect::None => return Verdict::Abandon,
        SideEffect::Started => TerminalState::StartedRetired,
        SideEffect::Committed => TerminalState::Committed,
        _ => return Verdict::Mismatch,
    };
    Verdict::Terminal(CachedTerminal {
        state,
        effect: view.effect,
        outcome: view.outcome.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::class_cache::DrainOutcome;
    use robot_msg::{build_helpers::*, msg::*};

    fn presence(
        table: flatbuffers::Table<'_>,
        offset: flatbuffers::VOffsetT,
        value: bool,
    ) -> FieldPresence<bool> {
        if table.vtable().get(offset) == 0 {
            FieldPresence::Absent
        } else {
            FieldPresence::Present(value)
        }
    }
    fn decode(method: Method, bytes: &[u8]) -> ReplyView<'_> {
        match method {
            Method::ClearEStop => {
                let r = flatbuffers::root::<ClearEStopResponse>(bytes).unwrap();
                ReplyView {
                    method,
                    request_id: r.request_id().unwrap(),
                    retired: presence(r._tab, ClearEStopResponse::VT_RETIRED, r.retired()),
                    effect: r.effect(),
                    success: r.status() == RpcStatus::Ok,
                    outcome: TypedOutcome::ClearEStop {
                        status: r.status(),
                        domain: r.domain(),
                        superseded: r.superseded(),
                        cleared_at_ns: r.cleared_at_ns(),
                        dry_run: r.dry_run(),
                        error_msg: r.error_msg().unwrap_or_default().to_owned(),
                    },
                }
            }
            Method::SetEnabled => {
                let r = flatbuffers::root::<SetEnabledResponse>(bytes).unwrap();
                ReplyView {
                    method,
                    request_id: r.request_id().unwrap(),
                    retired: presence(r._tab, SetEnabledResponse::VT_RETIRED, r.retired()),
                    effect: r.effect(),
                    success: r.status() == RpcStatus::Ok,
                    outcome: TypedOutcome::SetEnabled {
                        status: r.status(),
                        domain: r.domain(),
                        completed_at_ns: r.completed_at_ns(),
                        error_msg: r.error_msg().unwrap_or_default().to_owned(),
                    },
                }
            }
            Method::InvokeTask => {
                let r = flatbuffers::root::<InvokeTaskResponse>(bytes).unwrap();
                ReplyView {
                    method,
                    request_id: r.request_id().unwrap(),
                    retired: presence(r._tab, InvokeTaskResponse::VT_RETIRED, r.retired()),
                    effect: r.effect(),
                    success: r.status() == ActionRpcStatus::Ok,
                    outcome: TypedOutcome::InvokeTask {
                        status: r.status(),
                        action_id: r.action_id().unwrap_or_default().to_owned(),
                        accepted_at_ns: r.accepted_at_ns(),
                        error_msg: r.error_msg().unwrap_or_default().to_owned(),
                        drain: DrainOutcome::Registered,
                    },
                }
            }
            Method::CancelTask => {
                let r = flatbuffers::root::<CancelTaskResponse>(bytes).unwrap();
                ReplyView {
                    method,
                    request_id: r.request_id().unwrap(),
                    retired: presence(r._tab, CancelTaskResponse::VT_RETIRED, r.retired()),
                    effect: r.effect(),
                    success: r.status() == ActionRpcStatus::Ok,
                    outcome: TypedOutcome::CancelTask {
                        status: r.status(),
                        action_id: r.action_id().unwrap_or_default().to_owned(),
                        cancel_at_ns: r.cancel_at_ns(),
                        error_msg: r.error_msg().unwrap_or_default().to_owned(),
                    },
                }
            }
            Method::ClearError => {
                let r = flatbuffers::root::<ClearErrorResponse>(bytes).unwrap();
                ReplyView {
                    method,
                    request_id: r.request_id().unwrap(),
                    retired: presence(r._tab, ClearErrorResponse::VT_RETIRED, r.retired()),
                    effect: r.effect(),
                    success: r.status() == ActionRpcStatus::Ok,
                    outcome: TypedOutcome::ClearError {
                        status: r.status(),
                        cleared_at_ns: r.cleared_at_ns(),
                        error_msg: r.error_msg().unwrap_or_default().to_owned(),
                    },
                }
            }
            Method::EStop => panic!("EStop 发布不走 RR 回复"),
        }
    }
    fn expected(method: Method) -> Expected {
        Expected {
            method,
            request_id: "request".into(),
        }
    }
    fn raw_set(effect: SideEffect, retired: bool, force: bool) -> Vec<u8> {
        let mut fbb = flatbuffers::FlatBufferBuilder::new();
        fbb.force_defaults(force);
        let id = fbb.create_string("request");
        let response = SetEnabledResponse::create(
            &mut fbb,
            &SetEnabledResponseArgs {
                request_id: Some(id),
                status: RpcStatus::Ok,
                effect,
                retired,
                ..Default::default()
            },
        );
        fbb.finish(response, None);
        fbb.finished_data().to_vec()
    }
    fn typed(method: Method, effect: SideEffect, retired: bool, success: bool) -> Vec<u8> {
        let rpc = if success {
            RpcStatus::Ok
        } else {
            RpcStatus::DeadlineExceeded
        };
        let action = if success {
            ActionRpcStatus::Ok
        } else {
            ActionRpcStatus::Internal
        };
        match method {
            Method::ClearEStop => build_clear_estop_response_typed(
                "request",
                rpc,
                "原因",
                31,
                true,
                ClearResponseMeta {
                    domain: ClearEStopStatus(11),
                    superseded: true,
                    effect,
                    retired,
                },
            ),
            Method::SetEnabled => build_set_enabled_response_typed(
                "request",
                rpc,
                SetEnabledStatus(12),
                32,
                "原因",
                effect,
                retired,
            ),
            Method::InvokeTask => build_invoke_task_response_typed(
                "request", action, "原因", "action", 33, effect, retired,
            ),
            Method::CancelTask => build_cancel_task_response_typed(
                "request", action, "原因", "action", 34, effect, retired,
            ),
            Method::ClearError => {
                build_clear_error_response_typed("request", action, "原因", 35, effect, retired)
            }
            Method::EStop => panic!("EStop 发布不走 RR 回复"),
        }
    }
    const METHODS: [Method; 5] = [
        Method::ClearEStop,
        Method::SetEnabled,
        Method::InvokeTask,
        Method::CancelTask,
        Method::ClearError,
    ];

    #[test]
    fn request_id_mismatch_is_mismatch() {
        let bytes = typed(Method::SetEnabled, SideEffect::Committed, true, true);
        let view = decode(Method::SetEnabled, &bytes);
        assert_eq!(
            classify(
                &Expected {
                    method: Method::SetEnabled,
                    request_id: "other".into()
                },
                &view
            ),
            Verdict::Mismatch
        );
        assert_eq!(
            classify(&expected(Method::ClearEStop), &view),
            Verdict::Mismatch
        );
    }
    #[test]
    fn retired_absent_or_false_is_pending_even_with_illegal_effect() {
        for force in [false, true] {
            let bytes = raw_set(SideEffect(4), false, force);
            let view = decode(Method::SetEnabled, &bytes);
            assert_eq!(
                view.retired,
                if force {
                    FieldPresence::Present(false)
                } else {
                    FieldPresence::Absent
                }
            );
            assert_eq!(view.effect.0, 4);
            assert_eq!(
                classify(&expected(Method::SetEnabled), &view),
                Verdict::Pending
            );
        }
        for method in METHODS {
            let bytes = typed(method, SideEffect(4), false, true);
            let view = decode(method, &bytes);
            assert_eq!(view.retired, FieldPresence::Absent);
            assert_eq!(classify(&expected(method), &view), Verdict::Pending);
        }
    }
    #[test]
    fn old_wire_both_fields_absent_is_pending() {
        let mut fbb = flatbuffers::FlatBufferBuilder::new();
        let id = fbb.create_string("request");
        let old = robot_msg::v0::SetEnabledResponse::create(
            &mut fbb,
            &robot_msg::v0::SetEnabledResponseArgs {
                request_id: Some(id),
                status: robot_msg::v0::RpcStatus::Ok,
                ..Default::default()
            },
        );
        fbb.finish(old, None);
        let bytes = fbb.finished_data();
        let decoded = flatbuffers::root::<SetEnabledResponse>(bytes).unwrap();
        assert_eq!(decoded._tab.vtable().get(SetEnabledResponse::VT_EFFECT), 0);
        let view = decode(Method::SetEnabled, bytes);
        assert_eq!(view.retired, FieldPresence::Absent);
        assert_eq!(view.effect, SideEffect::Unknown);
        assert_eq!(
            classify(&expected(Method::SetEnabled), &view),
            Verdict::Pending
        );
    }
    #[test]
    fn retired_true_effect_omitted_and_explicit_zero_are_unknown_retired() {
        for force in [false, true] {
            let bytes = raw_set(SideEffect::Unknown, true, force);
            let response = flatbuffers::root::<SetEnabledResponse>(&bytes).unwrap();
            assert_eq!(
                response._tab.vtable().get(SetEnabledResponse::VT_EFFECT) != 0,
                force
            );
            let view = decode(Method::SetEnabled, &bytes);
            assert_eq!(view.retired, FieldPresence::Present(true));
            assert_eq!(
                classify(&expected(Method::SetEnabled), &view),
                Verdict::Terminal(CachedTerminal {
                    state: TerminalState::UnknownRetired,
                    effect: SideEffect::Unknown,
                    outcome: view.outcome.clone()
                })
            );
        }
    }
    #[test]
    fn retired_true_raw_effect_4_is_mismatch_no_release() {
        let bytes = raw_set(SideEffect(4), true, false);
        let view = decode(Method::SetEnabled, &bytes);
        assert_eq!(view.effect.0, 4);
        assert_eq!(view.retired, FieldPresence::Present(true));
        assert_eq!(
            classify(&expected(Method::SetEnabled), &view),
            Verdict::Mismatch
        );
    }
    #[test]
    fn retired_true_effect_matrix_none_started_committed() {
        for method in METHODS {
            for success in [false, true] {
                for (effect, state) in [
                    (SideEffect::None, TerminalState::Committed),
                    (SideEffect::Started, TerminalState::StartedRetired),
                    (SideEffect::Committed, TerminalState::Committed),
                    (SideEffect::Unknown, TerminalState::UnknownRetired),
                ] {
                    let bytes = typed(method, effect, true, success);
                    let view = decode(method, &bytes);
                    assert_eq!(view.retired, FieldPresence::Present(true));
                    let wanted = if effect == SideEffect::None && !success {
                        Verdict::Abandon
                    } else {
                        Verdict::Terminal(CachedTerminal {
                            state,
                            effect,
                            outcome: view.outcome.clone(),
                        })
                    };
                    assert_eq!(classify(&expected(method), &view), wanted);
                }
            }
        }
    }
}
