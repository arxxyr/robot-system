//! 原S10日志与handler结果的有限解析；不构造cache或退休凭证。
use super::{Case, Code, RequestResult, Value, json};

pub(super) fn handler(
    result: &RequestResult,
    code: Code,
    not_before: u64,
    until: u64,
) -> Result<Value, String> {
    let (observed, outcome) = result;
    let error = outcome.as_ref().err().ok_or("原未退休请求返回了成功Ack")?;
    if error.code() != code || *observed < not_before || *observed >= until {
        return Err(format!(
            "原handler code/实际接受时间不符：{error} at={observed} start={not_before} until={until}"
        ));
    }
    let expected = match code {
        Code::DeadlineExceeded => "no RR response within 5500 ms (timeout_ms + grace_ms)",
        Code::Aborted => "request remains in flight: Orphan",
        _ => return Err("非本包原handler分类".into()),
    };
    if error.message() != expected {
        return Err(format!("原handler错误不是预期来源：{}", error.message()));
    }
    Ok(
        json!({"code":format!("{code:?}"),"message":error.message(),"observed_at_ns":observed,"not_before_ns":not_before,"until_ns":until}),
    )
}

fn rows(logs: &str, marker: &str) -> Result<Vec<Value>, String> {
    logs.lines()
        .filter(|line| line.contains(marker))
        .map(|line| {
            let row: Value =
                serde_json::from_str(line).map_err(|e| format!("原记录不是完整JSON：{e}"))?;
            if row["message"] != marker {
                return Err("原marker不在message".into());
            }
            Ok(row)
        })
        .collect()
}

pub(super) fn original_deadline(logs: &str, request_id: &str) -> Result<u64, String> {
    let rows = rows(logs, "R4原ClearRR责任")?;
    let mut value = None;
    for row in rows.iter().filter(|row| row["request_id"] == request_id) {
        let raw = row["deadline_ns"].as_str().ok_or("原RR deadline字段缺失")?;
        let n = raw
            .strip_prefix("Some(")
            .and_then(|v| v.strip_suffix(')'))
            .ok_or("原RR无实际deadline")?
            .parse::<u64>()
            .map_err(|e| e.to_string())?;
        if n == 0 || value.is_some_and(|old| old != n) {
            return Err("同一原RR deadline改变或为零".into());
        }
        value = Some(n);
    }
    value.ok_or("未见本请求的原RR deadline".into())
}

pub(super) fn ledger(
    logs: &str,
    request_id: &str,
    key: &str,
    duplicate: bool,
) -> Result<Value, String> {
    let records = rows(logs, "R4C原网关责任")?;
    if records.len() != if duplicate { 2 } else { 1 } {
        return Err(format!("原网关定点记录数量不符：{}", records.len()));
    }
    for (index, row) in records.iter().enumerate() {
        if row["point"] != if index == 0 { "orphan" } else { "duplicate" }
            || row["method"] != "clear_estop"
            || row["request_id"] != request_id
            || row["key"] != key
            || row["state"] != "Orphan"
            || row["terminal"] != false
            || row["placeholder"] != true
            || row["wire"] != true
            || row["restore"] != true
            || row["cancel_ticket"] != false
            || row["admission_closed"] != false
            || row["faulted"] != false
            || row["entry_id"].as_str().is_none_or(str::is_empty)
            || row["cache_entry_id"] != row["entry_id"]
            || row["at_ns"].as_u64().is_none_or(|v| v == 0)
            || row["fingerprint"].as_str().is_none_or(str::is_empty)
        {
            return Err(format!("原同锁缓存/持有责任不符：{row}"));
        }
        if index == 1 {
            for field in [
                "entry_id",
                "cache_entry_id",
                "fingerprint",
                "key",
                "request_id",
                "placeholder",
                "wire",
                "restore",
                "cancel_ticket",
                "state",
                "terminal",
            ] {
                if row[field] != records[0][field] {
                    return Err(format!("重复判定改变原{field}"));
                }
            }
            if row["at_ns"].as_u64() <= records[0]["at_ns"].as_u64() {
                return Err("duplicate早于原Orphan".into());
            }
        }
    }
    Ok(json!(records))
}

pub(super) fn files(case: &Case) -> Result<(), String> {
    for path in [&case.latch, &case.directory.join("estop_latch.bak")] {
        if std::fs::read(path).map_err(|e| e.to_string())? != b"0\n" {
            return Err("原逻辑提交后文件不为0/0".into());
        }
    }
    match std::fs::symlink_metadata(case.directory.join("estop_latch.clear_pending")) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => Err(format!("原marker缺失不成立：{other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn handler_rejects_transport_timeout_wrong_source_and_both_deadline_edges() {
        let original = (
            15,
            Err(tonic::Status::deadline_exceeded(
                "no RR response within 5500 ms (timeout_ms + grace_ms)",
            )),
        );
        assert!(handler(&original, Code::DeadlineExceeded, 10, 20).is_ok());
        assert!(handler(&original, Code::DeadlineExceeded, 16, 20).is_err());
        assert!(handler(&original, Code::DeadlineExceeded, 10, 15).is_err());
        assert!(
            handler(
                &(15, Err(tonic::Status::deadline_exceeded("Timeout expired"))),
                Code::DeadlineExceeded,
                10,
                20
            )
            .is_err()
        );
        assert!(
            handler(
                &(
                    15,
                    Err(tonic::Status::aborted("request remains in flight: Sent"))
                ),
                Code::Aborted,
                10,
                20
            )
            .is_err()
        );
    }
    #[test]
    fn ledger_rejects_refund_terminal_new_entry_and_duplicate_multiplicity() {
        let a = json!({"message":"R4C原网关责任","point":"orphan","method":"clear_estop","request_id":"id","key":"key","state":"Orphan","terminal":false,"placeholder":true,"wire":true,"restore":true,"cancel_ticket":false,"admission_closed":false,"faulted":false,"entry_id":"EntryId(1)","cache_entry_id":"EntryId(1)","fingerprint":"[1]","at_ns":10});
        let mut b = a.clone();
        b["point"] = json!("duplicate");
        b["at_ns"] = json!(11);
        let logs = |b: &Value| format!("{a}\n{b}\n");
        assert!(ledger(&logs(&b), "id", "key", true).is_ok());
        for (field, value) in [
            ("wire", json!(false)),
            ("terminal", json!(true)),
            ("entry_id", json!("EntryId(2)")),
            ("placeholder", json!(false)),
            ("restore", json!(false)),
            ("fingerprint", json!("[2]")),
        ] {
            let mut bad = b.clone();
            bad[field] = value;
            assert!(ledger(&logs(&bad), "id", "key", true).is_err());
        }
        assert!(ledger(&format!("{}{}", logs(&b), b), "id", "key", true).is_err());
    }
    #[test]
    fn original_rr_deadline_requires_same_request_and_one_unchanged_actual_value() {
        let row = json!({"message":"R4原ClearRR责任","request_id":"id","deadline_ns":"Some(123)"});
        assert_eq!(original_deadline(&row.to_string(), "id").unwrap(), 123);
        assert!(original_deadline(&row.to_string(), "other").is_err());
        assert!(
            original_deadline(
                &format!(
                    "{row}\n{}",
                    json!({"message":"R4原ClearRR责任","request_id":"id","deadline_ns":"Some(124)"})
                ),
                "id"
            )
            .is_err()
        );
    }
}
