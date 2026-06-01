//! 输出格式（AC-7）：默认 human-readable + `--json` 切换。
//!
//! JSON schema 规则：`snake_case` 字段、timestamps 用 `<key>_ns` 整数、
//! enums 用 `ALL_CAPS` string、空字段不省略（schema-stable）。stderr 的
//! warning / info 永远 human-readable，**不**进 JSON（只 stdout 是 JSON）。

use robot_rpc::proto::{
    TaskEvent, TaskEventKind, TaskState, TaskStatus, action_failure_proto, action_outcome_proto,
};
use serde::Serialize;

/// 每个子命令的输出类型实现本 trait：`Serialize` 供 `--json`，
/// [`CliOutput::human`] 供默认 human-readable。
pub trait CliOutput: Serialize {
    /// 默认 human-readable 输出（单行或多行）。
    fn human(&self) -> String;
}

/// 按 `--json` 开关把输出写到 stdout（AC-7）。
///
/// (1/5) 作为共享渲染入口落地（pub API）；各子命令的输出 struct 随其 impl
/// commit 落地。stdout 行为由 `robotctl_smoke` `json_output_round_trip`
/// phase 覆盖（(5/5)）。
pub fn print_output<T: CliOutput>(value: &T, json: bool) {
    if json {
        match serde_json::to_string(value) {
            Ok(s) => println!("{s}"),
            // plain struct 序列化不会失败；保底不 panic（CLI 工具）。
            Err(e) => eprintln!("error: failed to serialize output as json: {e}"),
        }
    } else {
        println!("{}", value.human());
    }
}

/// `clear-estop` / `clear-error` 的 `Ack` 输出（AC-7）。两条 RPC 都返
/// proto `Ack`，输出形态一致。
///
/// JSON 字段序 = `cleared_at_ns` / `dry_run` / `request_id`（serde 按声明
/// 序，对齐 AC-7 schema）；human 省略 `request_id`
/// （`cleared_at_ns=<ns> dry_run=<bool>`）。
#[derive(Debug, Serialize)]
pub struct AckOutput {
    /// daemon 端 clear 完成时刻（`dry_run` 时为"would clear at"评估时刻）。
    pub cleared_at_ns: u64,
    /// 回传 `request.dry_run`，client audit 不需 lookup request。
    pub dry_run: bool,
    /// 回传 `CommandMeta.request_id`（client correlation）。
    pub request_id: String,
}

impl CliOutput for AckOutput {
    fn human(&self) -> String {
        format!(
            "cleared_at_ns={} dry_run={}",
            self.cleared_at_ns, self.dry_run
        )
    }
}

// ============================================================================
// 枚举 → 字符串 label（AC-7）。state / event 在 human + json 都用 ALL_CAPS；
// outcome / failure 的 kind 在 json 用 ALL_CAPS、human 用 friendly TitleCase
// （AC-7 human 示例 `outcome=Success`）。全 exhaustive 不留 `_`，新增 proto
// variant 编译 fail 强制更新。
// ============================================================================

fn task_state_label(s: TaskState) -> &'static str {
    match s {
        TaskState::Unspecified => "UNSPECIFIED",
        TaskState::Pending => "PENDING",
        TaskState::Executing => "EXECUTING",
        TaskState::Completed => "COMPLETED",
        TaskState::Failed => "FAILED",
        TaskState::Cancelled => "CANCELLED",
    }
}

fn event_kind_label(k: TaskEventKind) -> &'static str {
    match k {
        TaskEventKind::Unspecified => "UNSPECIFIED",
        TaskEventKind::Started => "STARTED",
        TaskEventKind::Progress => "PROGRESS",
        TaskEventKind::Completed => "COMPLETED",
        TaskEventKind::Failed => "FAILED",
        TaskEventKind::Cancelled => "CANCELLED",
    }
}

fn outcome_kind_label(k: action_outcome_proto::Kind) -> &'static str {
    match k {
        action_outcome_proto::Kind::Unspecified => "UNSPECIFIED",
        action_outcome_proto::Kind::Success => "SUCCESS",
        action_outcome_proto::Kind::PartialSuccess => "PARTIAL_SUCCESS",
    }
}

fn outcome_kind_friendly(k: action_outcome_proto::Kind) -> &'static str {
    match k {
        action_outcome_proto::Kind::Unspecified => "Unspecified",
        action_outcome_proto::Kind::Success => "Success",
        action_outcome_proto::Kind::PartialSuccess => "PartialSuccess",
    }
}

fn failure_kind_label(k: action_failure_proto::Kind) -> &'static str {
    match k {
        action_failure_proto::Kind::Unspecified => "UNSPECIFIED",
        action_failure_proto::Kind::DeadlineExceeded => "DEADLINE_EXCEEDED",
        action_failure_proto::Kind::PreconditionFailed => "PRECONDITION_FAILED",
        action_failure_proto::Kind::AdapterError => "ADAPTER_ERROR",
        action_failure_proto::Kind::SafetyAbort => "SAFETY_ABORT",
        action_failure_proto::Kind::InternalError => "INTERNAL_ERROR",
    }
}

fn failure_kind_friendly(k: action_failure_proto::Kind) -> &'static str {
    match k {
        action_failure_proto::Kind::Unspecified => "Unspecified",
        action_failure_proto::Kind::DeadlineExceeded => "DeadlineExceeded",
        action_failure_proto::Kind::PreconditionFailed => "PreconditionFailed",
        action_failure_proto::Kind::AdapterError => "AdapterError",
        action_failure_proto::Kind::SafetyAbort => "SafetyAbort",
        action_failure_proto::Kind::InternalError => "InternalError",
    }
}

/// `ActionOutcome` 的 JSON 形态（COMPLETED 态；AC-7：`kind` `ALL_CAPS` +
/// `warning` 不省略）。
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct OutcomeJson {
    /// `ALL_CAPS`（`SUCCESS` / `PARTIAL_SUCCESS` / `UNSPECIFIED`）。
    pub kind: String,
    /// `PARTIAL_SUCCESS` 时非空；其它留空（不省略）。
    pub warning: String,
}

/// `ActionFailure` 的 JSON 形态（FAILED 态；AC-7）。
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct FailureJson {
    /// `ALL_CAPS`（`ADAPTER_ERROR` / `DEADLINE_EXCEEDED` / ...）。
    pub kind: String,
    /// `DEADLINE_EXCEEDED` 留空；其它带具体 reason。
    pub reason: String,
}

/// `get-task-status` 输出（AC-7 三态 + PENDING / CANCELLED）。
///
/// JSON 形态按 state 条件化：`progress` 仅 EXECUTING、`outcome` 仅
/// COMPLETED、`failure` 仅 FAILED（`skip_serializing_if` 省略其余）；
/// `state` / `action_id` / `request_id` 恒在。`detail_human` 是预渲染的
/// human 中段（不进 JSON）。
#[derive(Debug, Serialize)]
pub struct TaskStatusOutput {
    /// `ALL_CAPS` state。
    pub state: String,
    /// 仅 EXECUTING。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<f32>,
    /// 仅 COMPLETED。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<OutcomeJson>,
    /// 仅 FAILED。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<FailureJson>,
    /// echo 回填。
    pub action_id: String,
    /// echo 回填。
    pub request_id: String,
    /// human 中段（`progress=..` / `outcome=Success` / `failure=.. reason=..`
    /// / 空）；`#[serde(skip)]` 不进 JSON。
    #[serde(skip)]
    detail_human: String,
}

impl TaskStatusOutput {
    /// 从 proto `TaskStatus` 构造（纯映射；lib unit 锁 human + json）。
    #[must_use]
    pub fn from_proto(s: &TaskStatus) -> Self {
        let state_enum = TaskState::try_from(s.state).unwrap_or(TaskState::Unspecified);
        let (mut progress, mut outcome, mut failure, mut detail_human) =
            (None, None, None, String::new());
        match state_enum {
            TaskState::Executing => {
                progress = Some(s.progress);
                detail_human = format!("progress={}", s.progress);
            }
            TaskState::Completed => {
                let oc = s.outcome.clone().unwrap_or_default();
                let k = action_outcome_proto::Kind::try_from(oc.kind)
                    .unwrap_or(action_outcome_proto::Kind::Unspecified);
                detail_human = format!("outcome={}", outcome_kind_friendly(k));
                outcome = Some(OutcomeJson {
                    kind: outcome_kind_label(k).to_owned(),
                    warning: oc.warning,
                });
            }
            TaskState::Failed => {
                let f = s.failure.clone().unwrap_or_default();
                let k = action_failure_proto::Kind::try_from(f.kind)
                    .unwrap_or(action_failure_proto::Kind::Unspecified);
                detail_human =
                    format!("failure={} reason={:?}", failure_kind_friendly(k), f.reason);
                failure = Some(FailureJson {
                    kind: failure_kind_label(k).to_owned(),
                    reason: f.reason,
                });
            }
            TaskState::Pending | TaskState::Cancelled | TaskState::Unspecified => {}
        }
        Self {
            state: task_state_label(state_enum).to_owned(),
            progress,
            outcome,
            failure,
            action_id: s.action_id.clone(),
            request_id: s.request_id.clone(),
            detail_human,
        }
    }
}

impl CliOutput for TaskStatusOutput {
    fn human(&self) -> String {
        if self.detail_human.is_empty() {
            format!("state={} action_id={}", self.state, self.action_id)
        } else {
            format!(
                "state={} {} action_id={}",
                self.state, self.detail_human, self.action_id
            )
        }
    }
}

/// `cancel-task` 输出（AC-7）：Ack 但**不含** `dry_run`（cancel 不带 dry-run
/// 语义）。
#[derive(Debug, Serialize)]
pub struct CancelAckOutput {
    /// cancel 受理时刻。
    pub cleared_at_ns: u64,
    /// echo 回填。
    pub request_id: String,
}

impl CliOutput for CancelAckOutput {
    fn human(&self) -> String {
        format!("cleared_at_ns={}", self.cleared_at_ns)
    }
}

/// `stream-events` 单帧输出（AC-7 / AC-10；NDJSON 一行一帧）。
///
/// 条件字段同 [`TaskStatusOutput`]：`progress` 仅 PROGRESS、`outcome` 仅
/// COMPLETED、`failure` 仅 FAILED。human 帧**不含** `action_id`（AC-7 帧示例
/// `at_ns=.. event=..`）。
#[derive(Debug, Serialize)]
pub struct TaskEventOutput {
    /// 事件时刻。
    pub at_ns: u64,
    /// `ALL_CAPS` event。
    pub event: String,
    /// 仅 PROGRESS。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<f32>,
    /// 仅 COMPLETED。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<OutcomeJson>,
    /// 仅 FAILED。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<FailureJson>,
    /// echo 回填。
    pub action_id: String,
    /// echo 回填。
    pub request_id: String,
    /// human 中段（不进 JSON）。
    #[serde(skip)]
    detail_human: String,
}

impl TaskEventOutput {
    /// 从 proto `TaskEvent` 构造（纯映射）。
    #[must_use]
    pub fn from_proto(ev: &TaskEvent) -> Self {
        let kind = TaskEventKind::try_from(ev.event).unwrap_or(TaskEventKind::Unspecified);
        let (mut progress, mut outcome, mut failure, mut detail_human) =
            (None, None, None, String::new());
        match kind {
            TaskEventKind::Progress => {
                progress = Some(ev.progress);
                detail_human = format!("progress={}", ev.progress);
            }
            TaskEventKind::Completed => {
                let oc = ev.outcome.clone().unwrap_or_default();
                let k = action_outcome_proto::Kind::try_from(oc.kind)
                    .unwrap_or(action_outcome_proto::Kind::Unspecified);
                detail_human = format!("outcome={}", outcome_kind_friendly(k));
                outcome = Some(OutcomeJson {
                    kind: outcome_kind_label(k).to_owned(),
                    warning: oc.warning,
                });
            }
            TaskEventKind::Failed => {
                let f = ev.failure.clone().unwrap_or_default();
                let k = action_failure_proto::Kind::try_from(f.kind)
                    .unwrap_or(action_failure_proto::Kind::Unspecified);
                detail_human =
                    format!("failure={} reason={:?}", failure_kind_friendly(k), f.reason);
                failure = Some(FailureJson {
                    kind: failure_kind_label(k).to_owned(),
                    reason: f.reason,
                });
            }
            TaskEventKind::Started | TaskEventKind::Cancelled | TaskEventKind::Unspecified => {}
        }
        Self {
            at_ns: ev.at_ns,
            event: event_kind_label(kind).to_owned(),
            progress,
            outcome,
            failure,
            action_id: ev.action_id.clone(),
            request_id: ev.request_id.clone(),
            detail_human,
        }
    }

    /// 是否终态帧（COMPLETED / FAILED / CANCELLED）—— stream 收到即关闭。
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self.event.as_str(), "COMPLETED" | "FAILED" | "CANCELLED")
    }
}

impl CliOutput for TaskEventOutput {
    fn human(&self) -> String {
        if self.detail_human.is_empty() {
            format!("at_ns={} event={}", self.at_ns, self.event)
        } else {
            format!(
                "at_ns={} event={} {}",
                self.at_ns, self.event, self.detail_human
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use robot_rpc::proto::{
        ActionFailureProto, ActionOutcomeProto, TaskEvent, TaskEventKind, TaskState, TaskStatus,
        action_failure_proto, action_outcome_proto,
    };
    use serde::Serialize;

    use super::{AckOutput, CancelAckOutput, CliOutput, TaskEventOutput, TaskStatusOutput};

    #[derive(Serialize)]
    struct Dummy {
        request_id: String,
        at_ns: u64,
        event: &'static str,
    }

    impl CliOutput for Dummy {
        fn human(&self) -> String {
            format!(
                "at_ns={} event={} request_id={}",
                self.at_ns, self.event, self.request_id
            )
        }
    }

    #[test]
    fn json_is_snake_case_ns_int_caps_enum_no_omit() {
        let d = Dummy {
            request_id: "rid".to_owned(),
            at_ns: 123,
            event: "STARTED",
        };
        let s = serde_json::to_string(&d).expect("serialize");
        assert_eq!(s, r#"{"request_id":"rid","at_ns":123,"event":"STARTED"}"#);
    }

    #[test]
    fn human_is_key_value() {
        let d = Dummy {
            request_id: "rid".to_owned(),
            at_ns: 123,
            event: "STARTED",
        };
        assert_eq!(d.human(), "at_ns=123 event=STARTED request_id=rid");
    }

    #[test]
    fn ack_output_human_omits_request_id() {
        let a = AckOutput {
            cleared_at_ns: 1_234_567_890,
            dry_run: false,
            request_id: "rid-1".to_owned(),
        };
        assert_eq!(a.human(), "cleared_at_ns=1234567890 dry_run=false");
    }

    #[test]
    fn ack_output_json_is_schema_stable() {
        let a = AckOutput {
            cleared_at_ns: 1_234_567_890,
            dry_run: false,
            request_id: "rid-1".to_owned(),
        };
        let s = serde_json::to_string(&a).expect("serialize");
        assert_eq!(
            s,
            r#"{"cleared_at_ns":1234567890,"dry_run":false,"request_id":"rid-1"}"#
        );
    }

    #[test]
    fn ack_output_dry_run_true_renders_both() {
        let a = AckOutput {
            cleared_at_ns: 42,
            dry_run: true,
            request_id: "rid-2".to_owned(),
        };
        assert_eq!(a.human(), "cleared_at_ns=42 dry_run=true");
        let s = serde_json::to_string(&a).expect("serialize");
        assert_eq!(
            s,
            r#"{"cleared_at_ns":42,"dry_run":true,"request_id":"rid-2"}"#
        );
    }

    // ---- get-task-status TaskStatusOutput（AC-7 三态 + PENDING）----

    fn status(state: TaskState) -> TaskStatus {
        TaskStatus {
            request_id: "rid".to_owned(),
            action_id: "aid".to_owned(),
            state: state as i32,
            progress: 0.0,
            outcome: None,
            failure: None,
        }
    }

    #[test]
    fn task_status_executing_human_and_json() {
        let mut s = status(TaskState::Executing);
        s.progress = 0.5;
        let out = TaskStatusOutput::from_proto(&s);
        assert_eq!(out.human(), "state=EXECUTING progress=0.5 action_id=aid");
        assert_eq!(
            serde_json::to_string(&out).expect("json"),
            r#"{"state":"EXECUTING","progress":0.5,"action_id":"aid","request_id":"rid"}"#
        );
    }

    #[test]
    fn task_status_completed_success_human_and_json() {
        let mut s = status(TaskState::Completed);
        s.outcome = Some(ActionOutcomeProto {
            kind: action_outcome_proto::Kind::Success as i32,
            warning: String::new(),
        });
        let out = TaskStatusOutput::from_proto(&s);
        assert_eq!(out.human(), "state=COMPLETED outcome=Success action_id=aid");
        assert_eq!(
            serde_json::to_string(&out).expect("json"),
            r#"{"state":"COMPLETED","outcome":{"kind":"SUCCESS","warning":""},"action_id":"aid","request_id":"rid"}"#
        );
    }

    #[test]
    fn task_status_failed_adapter_error_human_and_json() {
        let mut s = status(TaskState::Failed);
        s.failure = Some(ActionFailureProto {
            kind: action_failure_proto::Kind::AdapterError as i32,
            reason: "boom".to_owned(),
        });
        let out = TaskStatusOutput::from_proto(&s);
        assert_eq!(
            out.human(),
            r#"state=FAILED failure=AdapterError reason="boom" action_id=aid"#
        );
        assert_eq!(
            serde_json::to_string(&out).expect("json"),
            r#"{"state":"FAILED","failure":{"kind":"ADAPTER_ERROR","reason":"boom"},"action_id":"aid","request_id":"rid"}"#
        );
    }

    #[test]
    fn task_status_pending_simple_human_and_json() {
        let out = TaskStatusOutput::from_proto(&status(TaskState::Pending));
        assert_eq!(out.human(), "state=PENDING action_id=aid");
        assert_eq!(
            serde_json::to_string(&out).expect("json"),
            r#"{"state":"PENDING","action_id":"aid","request_id":"rid"}"#
        );
    }

    // ---- cancel-task CancelAckOutput（无 dry_run）----

    #[test]
    fn cancel_ack_output_human_and_json() {
        let a = CancelAckOutput {
            cleared_at_ns: 42,
            request_id: "rid".to_owned(),
        };
        assert_eq!(a.human(), "cleared_at_ns=42");
        assert_eq!(
            serde_json::to_string(&a).expect("json"),
            r#"{"cleared_at_ns":42,"request_id":"rid"}"#
        );
    }

    // ---- stream-events TaskEventOutput（AC-7 帧 / AC-10）----

    fn event(kind: TaskEventKind, at_ns: u64) -> TaskEvent {
        TaskEvent {
            request_id: "rid".to_owned(),
            action_id: "aid".to_owned(),
            at_ns,
            event: kind as i32,
            progress: 0.0,
            outcome: None,
            failure: None,
        }
    }

    #[test]
    fn task_event_started_human_and_json_no_action_id_in_human() {
        let out = TaskEventOutput::from_proto(&event(TaskEventKind::Started, 10));
        assert_eq!(out.human(), "at_ns=10 event=STARTED");
        assert!(!out.is_terminal());
        assert_eq!(
            serde_json::to_string(&out).expect("json"),
            r#"{"at_ns":10,"event":"STARTED","action_id":"aid","request_id":"rid"}"#
        );
    }

    #[test]
    fn task_event_progress_human_and_json() {
        let mut e = event(TaskEventKind::Progress, 20);
        e.progress = 0.5;
        let out = TaskEventOutput::from_proto(&e);
        assert_eq!(out.human(), "at_ns=20 event=PROGRESS progress=0.5");
        assert_eq!(
            serde_json::to_string(&out).expect("json"),
            r#"{"at_ns":20,"event":"PROGRESS","progress":0.5,"action_id":"aid","request_id":"rid"}"#
        );
    }

    #[test]
    fn task_event_completed_terminal_human_and_json() {
        let mut e = event(TaskEventKind::Completed, 30);
        e.outcome = Some(ActionOutcomeProto {
            kind: action_outcome_proto::Kind::Success as i32,
            warning: String::new(),
        });
        let out = TaskEventOutput::from_proto(&e);
        assert_eq!(out.human(), "at_ns=30 event=COMPLETED outcome=Success");
        assert!(out.is_terminal());
        assert_eq!(
            serde_json::to_string(&out).expect("json"),
            r#"{"at_ns":30,"event":"COMPLETED","outcome":{"kind":"SUCCESS","warning":""},"action_id":"aid","request_id":"rid"}"#
        );
    }

    #[test]
    fn task_event_partial_success_warning_propagates() {
        let mut e = event(TaskEventKind::Completed, 40);
        e.outcome = Some(ActionOutcomeProto {
            kind: action_outcome_proto::Kind::PartialSuccess as i32,
            warning: "clipped".to_owned(),
        });
        let out = TaskEventOutput::from_proto(&e);
        assert_eq!(
            out.human(),
            "at_ns=40 event=COMPLETED outcome=PartialSuccess"
        );
        assert_eq!(
            serde_json::to_string(&out).expect("json"),
            r#"{"at_ns":40,"event":"COMPLETED","outcome":{"kind":"PARTIAL_SUCCESS","warning":"clipped"},"action_id":"aid","request_id":"rid"}"#
        );
    }
}
