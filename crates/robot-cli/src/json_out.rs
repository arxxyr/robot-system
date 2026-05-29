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

#[cfg(test)]
mod tests {
    use super::CliOutput;
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
}
