//! 输出格式（AC-7）：默认 human-readable + `--json` 切换。
//!
//! JSON schema 规则：`snake_case` 字段、timestamps 用 `<key>_ns` 整数、
//! enums 用 `ALL_CAPS` string、空字段不省略（schema-stable）。stderr 的
//! warning / info 永远 human-readable，**不**进 JSON（只 stdout 是 JSON）。

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

#[cfg(test)]
mod tests {
    use super::{AckOutput, CliOutput};
    use serde::Serialize;

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
}
