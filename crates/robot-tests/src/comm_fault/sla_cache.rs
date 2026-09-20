//! S48 长负载的原缓存证据判读；不构建缓存模型或更改原负载结论。

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use super::harness::Case;
use super::sla_background::RR_PERIOD_NS;

const MARKER: &str = "S48原缓存准入";
const TTL_NS: u64 = 300_000_000_000;
const CLASSES: &str = "Emergency,Lower,Restore,Normal";
const STAGES: &str = "Queued,Sending,Publishing,Sent,Orphan,RepliedUnretired,NoHandle,Committed,StartedRetired,UnknownRetired";

type Identity = (String, String, String);

fn number(row: &Value, field: &str) -> Result<u64, String> {
    row[field]
        .as_u64()
        .ok_or_else(|| format!("字段{field}不是非负整数"))
}

fn string<'a>(row: &'a Value, field: &str) -> Result<&'a str, String> {
    row[field]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("字段{field}缺失或为空"))
}

fn array<'a>(row: &'a Value, field: &str) -> Result<&'a [Value], String> {
    row[field]
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| format!("字段{field}不是数组"))
}

fn identity(method: &str, request: &Value, key_field: &str) -> Result<Identity, String> {
    Ok((
        method.to_owned(),
        string(request, "request_id")?.to_owned(),
        string(request, key_field)?.to_owned(),
    ))
}

fn threshold(entries: u64, class: &str) -> Result<u64, String> {
    let reserved = match class {
        "Emergency" => 0,
        "Lower" => 64,
        "Restore" => 128,
        "Normal" => 256,
        other => return Err(format!("未知实际class：{other}")),
    };
    entries
        .checked_sub(reserved)
        .ok_or("原容量减预留溢出".into())
}

fn checked_snapshot(row: &Value, entries: u64) -> Result<(), String> {
    if row["message"] != MARKER
        || row["point"] != "post_gc_pre_admission"
        || row["classes"] != CLASSES
        || row["stages"] != STAGES
        || number(row, "max_entries")? != entries
        || number(row, "ttl_ns")? != TTL_NS
        || number(row, "now_ns")? == 0
    {
        return Err("原缓存记录位置、配置、分类标签或时间不符".into());
    }
    let class = string(row, "incoming_class")?;
    let bound = threshold(entries, class)?;
    let before = number(row, "before_gc_len")?;
    let removed = number(row, "gc_removed")?;
    let after = number(row, "post_gc_len")?;
    let matrix: [[u64; 10]; 4] =
        serde_json::from_str(string(row, "class_stage")?).map_err(|e| e.to_string())?;
    let total = matrix.iter().flatten().try_fold(0_u64, |total, value| {
        total.checked_add(*value).ok_or("原联合矩阵总数溢出")
    })?;
    if before > entries
        || before.checked_sub(removed) != Some(after)
        || total != after
        || number(row, "threshold")? != bound
        || (removed == 0 && class != "Lower" && after < bound)
    {
        return Err("原GC前后总数、联合矩阵或稀疏记录条件不符".into());
    }
    match string(row, "decision")? {
        "Capacity" => {
            if after < bound
                || row["capacity_class"] != format!("Some({class})")
                || row["capacity_len"] != format!("Some({after})")
                || row["capacity_threshold"] != format!("Some({bound})")
                || row["entry_id"] != "None"
            {
                return Err("真实Capacity分类/len/threshold与原GC后快照不一致".into());
            }
        }
        "Fresh" => {
            let valid_id = string(row, "entry_id")?
                .strip_prefix("Some(EntryId(")
                .and_then(|value| value.strip_suffix("))"))
                .and_then(|value| value.parse::<u64>().ok())
                .is_some_and(|value| value != 0);
            if after >= bound || !valid_id {
                return Err("真实Fresh在门外或缺少原EntryId".into());
            }
        }
        other => return Err(format!("全新key出现非Fresh/Capacity决定：{other}")),
    }
    Ok(())
}

struct Request<'a> {
    class: &'static str,
    row: &'a Value,
    response: &'a Value,
}

fn requests<'a>(
    background: &'a Value,
    recovery: &'a [Value],
    final_stop: &'a Value,
) -> Result<BTreeMap<Identity, Request<'a>>, String> {
    let mut result = BTreeMap::new();
    let mut keys = BTreeSet::new();
    let mut ids = BTreeSet::new();
    let mut insert = |method: &str, class, row: &'a Value, response: &'a Value, meta: &Value| {
        let id = identity(method, meta, "idempotency_key")?;
        if !keys.insert(id.2.clone())
            || !ids.insert(id.1.clone())
            || result
                .insert(
                    id,
                    Request {
                        class,
                        row,
                        response,
                    },
                )
                .is_some()
        {
            return Err("本次真实请求key/身份重复".to_owned());
        }
        Ok(())
    };
    for row in array(background, "rr_requests")? {
        insert("set_enabled", "Normal", row, &row["response"], row)?;
    }
    for row in recovery {
        let method = match string(row, "method")? {
            "ClearEStop" => "clear_estop",
            "SetEnabled(true)" => "set_enabled",
            other => return Err(format!("恢复写种类不符：{other}")),
        };
        insert(method, "Restore", row, &row["response"], row)?;
    }
    let lower = &final_stop["disable"];
    insert("set_enabled", "Lower", lower, lower, &lower["request"])?;
    Ok(result)
}

fn observations(
    logs: &str,
    entries: u64,
    requests: &BTreeMap<Identity, Request<'_>>,
) -> Result<BTreeMap<Identity, Value>, String> {
    let mut result = BTreeMap::new();
    for line in logs.lines().filter(|line| line.contains(MARKER)) {
        let row: Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
        checked_snapshot(&row, entries)?;
        let id = identity(string(&row, "method")?, &row, "key")?;
        let request = requests.get(&id).ok_or("原缓存记录找不到本次真实RPC")?;
        let at = number(&row, "now_ns")?;
        if row["incoming_class"] != request.class
            || at < number(request.row, "started_ns")?
            || at > number(request.row, "finished_ns")?
        {
            return Err("原缓存记录class/时刻与真实RPC不匹配".into());
        }
        if request.response["ok"] == true && row["decision"] != "Fresh" {
            return Err("成功新写没有对应真实Fresh决定".into());
        }
        if result.insert(id, row).is_some() {
            return Err("同一次RPC出现重复的原缓存准入记录".into());
        }
    }
    Ok(result)
}

fn valid_ack(row: &Value, request_id: &str, dry_run: bool) -> bool {
    row["ok"] == true
        && row["kind"] == "ack"
        && row["request_id"] == request_id
        && row["dry_run"] == dry_run
        && row["cleared_at_ns"].as_u64().is_some_and(|at| at != 0)
}

fn common_checks(background: &Value, final_stop: &Value, lifecycle: &Value) -> Result<(), String> {
    for field in ["final_stop_ok", "gateway_stop_ok", "s0_stop_ok"] {
        if lifecycle[field] != true {
            return Err(format!("实际收尾/退出未完成：{field}"));
        }
    }
    let lower = &final_stop["disable"];
    if final_stop["ok"] != true
        || final_stop["get_state"]["ok"] != true
        || final_stop["get_state"]["safety_state"] != 3
        || lower["attempted"] != true
        || lower["request"]["dry_run"] != false
        || lower["request"]["enabled"] != false
        || !valid_ack(lower, string(&lower["request"], "request_id")?, false)
    {
        return Err("真实state3或非dry Lower Ack未完成".into());
    }
    let stop = number(background, "stop_ns")?;
    let drained = number(background, "drained_ns")?;
    if background["valid_cadence"] != true
        || !background["admission_rejection"].is_null()
        || background["rr_target_per_second"] != 20
        || background["tracked_limit"] != 64
        || background["client_timeout_ns"] != 2_000_000_000_u64
        || background["drain_budget_ns"] != 2_500_000_000_u64
        || number(background, "peak_tracked")? > 64
        || number(background, "completed")? != number(background, "launched")?
        || background["launched"] != array(background, "rr_requests")?.len()
        || !array(background, "unfinished")?.is_empty()
        || stop == 0
        || drained
            .checked_sub(stop)
            .is_none_or(|elapsed| elapsed > 2_500_000_000)
    {
        return Err("原背景节拍、全部任务终结或固定drain预算不满足".into());
    }
    Ok(())
}

fn capacity_request(row: &Value, snapshots: &BTreeMap<Identity, Value>) -> Result<(), String> {
    let id = identity("set_enabled", row, "idempotency_key")?;
    let snapshot = snapshots.get(&id).ok_or("失败RPC缺原Capacity快照")?;
    let response = &row["response"];
    let after = number(snapshot, "post_gc_len")?;
    if snapshot["incoming_class"] != "Normal"
        || snapshot["decision"] != "Capacity"
        || snapshot["threshold"] != 3840
        || after < 3840
        || response["kind"] != "status"
        || response["code"] != "ResourceExhausted"
        || response["message"] != format!("idempotency cache full ({after}/3840)")
        || row["ok"] != false
    {
        return Err("原失败并非本次Normal缓存Capacity来源".into());
    }
    Ok(())
}

fn expected_red(
    background: &Value,
    snapshots: &BTreeMap<Identity, Value>,
    lifecycle: &Value,
) -> Result<(), String> {
    let first = &background["first_failure"];
    let failures = array(background, "failures")?;
    let rows = array(background, "rr_requests")?;
    let source = &first["source"];
    let index = usize::try_from(number(source, "sequence")?).map_err(|e| e.to_string())?;
    let row = rows.get(index).ok_or("首错sequence超出原请求记录")?;
    if source["kind"] != "request"
        || identity("set_enabled", source, "idempotency_key")?
            != identity("set_enabled", row, "idempotency_key")?
        || failures.first() != Some(first)
        || lifecycle["measurement_ok"] != false
        || lifecycle["measurement_error"] != format!("背景负载首错：{first}")
    {
        return Err("原测量并非该背景Capacity首错，或另有未解释测量错误".into());
    }
    capacity_request(row, snapshots)?;
    for failure in failures {
        let source = &failure["source"];
        let index = usize::try_from(number(source, "sequence")?).map_err(|e| e.to_string())?;
        let row = rows.get(index).ok_or("追加首错链sequence越界")?;
        if source["kind"] != "request"
            || identity("set_enabled", source, "idempotency_key")?
                != identity("set_enabled", row, "idempotency_key")?
            || failure["error"]
                != format!(
                    "请求 {index} / {}：{}",
                    string(row, "request_id")?,
                    row["response"]
                )
        {
            return Err("出现无法解释的监督/任务/响应错误".into());
        }
        match row["response"]["kind"].as_str() {
            Some("status") => capacity_request(row, snapshots)?,
            Some("not_sent")
                if row["response"]["error"] == format!("背景负载首错：{first}")
                    && number(row, "started_ns")? >= number(first, "stamp_ns")? => {}
            _ => return Err("Capacity停止链中出现其他错误".into()),
        }
    }
    for row in rows.iter().filter(|row| row["ok"] != true) {
        if !failures
            .iter()
            .any(|failure| failure["source"]["request_id"] == row["request_id"])
        {
            return Err("失败请求没有真实首错链记录".into());
        }
    }
    Ok(())
}

struct Inputs<'a> {
    background: &'a Value,
    recovery: &'a [Value],
    final_stop: &'a Value,
    summary: &'a Value,
    lifecycle: &'a Value,
    attempts: &'a [Value],
    logs: &'a str,
}

fn accept(entries: u64, input: &Inputs<'_>) -> Result<Value, String> {
    let Inputs {
        background,
        recovery,
        final_stop,
        summary,
        lifecycle,
        attempts,
        logs,
    } = input;
    if !matches!(entries, 4096 | 8192) {
        return Err("长负载仅支持原4096/8192".into());
    }
    common_checks(background, final_stop, lifecycle)?;
    let requests = requests(background, recovery, final_stop)?;
    let snapshots = observations(logs, entries, &requests)?;
    let lower_id = identity(
        "set_enabled",
        &final_stop["disable"]["request"],
        "idempotency_key",
    )?;
    if snapshots
        .get(&lower_id)
        .is_none_or(|row| row["decision"] != "Fresh")
    {
        return Err("最终Lower缺少原Fresh决定".into());
    }
    let (normal_keys, restore_keys) = successful_keys(background, recovery)?;
    let total = normal_keys.len() + restore_keys.len();
    let all_keys: BTreeSet<_> = normal_keys.union(&restore_keys).copied().collect();
    if all_keys.len() != total
        || summary["successful_unique_count"] != total
        || summary["successful_unique_keys"] != json!(all_keys)
        || summary["trace_dropped"] != 0
        || summary["measurement_error"] != lifecycle["measurement_error"]
    {
        return Err("唯一成功集合、原测量错误或trace丢失不符".into());
    }
    for attempt in *attempts {
        if attempt["invalid"] == true
            && (attempt["disarm_ok"] != true
                || attempt["dropped"] != 0
                || attempt["error"] != lifecycle["measurement_error"])
        {
            return Err("采样还存在独立Disarm/事件/测量错误".into());
        }
    }
    let gc_removed = snapshots
        .iter()
        .filter(|(id, _)| **id != lower_id)
        .try_fold(0_u64, |sum, (id, row)| {
            let removed = number(row, "gc_removed")?;
            if removed != 0
                && (requests[id].response["ok"] != true
                    || number(row, "now_ns")?
                        .checked_sub(number(background, "started_ns")?)
                        .is_none_or(|elapsed| elapsed < TTL_NS))
            {
                return Err("真实GC不是本轮跨TTL的成功测量请求".to_owned());
            }
            if number(row, "now_ns")? > number(background, "stop_ns")? {
                return Ok(sum);
            }
            sum.checked_add(removed).ok_or("GC总数溢出".to_owned())
        })?;
    let verdict = match entries {
        4096 => {
            expected_red(background, &snapshots, lifecycle)?;
            "EXPECTED_RED"
        }
        8192 => {
            green(background, summary, lifecycle, gc_removed, total)?;
            "GREEN"
        }
        _ => unreachable!(),
    };
    Ok(
        json!({"verdict":verdict,"max_entries":entries,"normal_threshold":threshold(entries,"Normal")?,
        "normal_successful_unique_keys":normal_keys,"restore_successful_unique_keys":restore_keys,
        "successful_unique_count_excluding_final_lower":total,"gc_removed":gc_removed,
        "final_lower_gc_removed":snapshots[&lower_id]["gc_removed"],
        "snapshot_count":snapshots.len(),"cache_decisions":snapshots.values().collect::<Vec<_>>(),
        "final_lower":snapshots[&lower_id],"raw_measurement_ok":lifecycle["measurement_ok"],
        "no_uart":true,"physical_p99_claim":false}),
    )
}

fn successful_keys<'a>(
    background: &'a Value,
    recovery: &'a [Value],
) -> Result<(BTreeSet<&'a str>, BTreeSet<&'a str>), String> {
    let mut normal = BTreeSet::new();
    let mut restore = BTreeSet::new();
    let started = number(background, "started_ns")?;
    for (index, row) in array(background, "rr_requests")?.iter().enumerate() {
        let due = u64::try_from(index)
            .ok()
            .and_then(|i| i.checked_mul(RR_PERIOD_NS))
            .and_then(|offset| started.checked_add(offset))
            .ok_or("原due计算溢出")?;
        let actual = number(row, "started_ns")?;
        let finished = number(row, "finished_ns")?;
        if row["sequence"] != index
            || number(row, "due_ns")? != due
            || actual
                .checked_sub(due)
                .is_none_or(|lag| lag >= RR_PERIOD_NS)
            || finished
                .checked_sub(actual)
                .is_none_or(|elapsed| elapsed > 2_000_000_000)
            || row["terminal"] != true
        {
            return Err("原背景RPC序号/节拍/实际期限/终结不满足".into());
        }
        if row["ok"] == true {
            if !valid_ack(&row["response"], string(row, "request_id")?, true) {
                return Err("背景成功行并非原有效Ack".into());
            }
            normal.insert(string(row, "idempotency_key")?);
        }
    }
    for row in recovery {
        if row["ok"] != true || !valid_ack(&row["response"], string(row, "request_id")?, false) {
            return Err("原live恢复写存在失败".into());
        }
        restore.insert(string(row, "idempotency_key")?);
    }
    Ok((normal, restore))
}

fn green(
    background: &Value,
    summary: &Value,
    lifecycle: &Value,
    removed: u64,
    count: usize,
) -> Result<(), String> {
    let elapsed = number(background, "stop_ns")?
        .checked_sub(number(background, "started_ns")?)
        .ok_or("真实负载时间倒序")?;
    if lifecycle["measurement_ok"] != true
        || !lifecycle["measurement_error"].is_null()
        || background["valid_responses"] != true
        || !background["first_failure"].is_null()
        || array(background, "rr_requests")?
            .iter()
            .any(|row| row["ok"] != true)
        || !array(background, "failures")?.is_empty()
        || !(360_000_000_000..=600_000_000_000).contains(&elapsed)
        || count <= 8192
        || removed == 0
        || summary["minimum_ns"] != 360_000_000_000_u64
        || summary["maximum_ns"] != 600_000_000_000_u64
        || summary["started_ns"] != background["started_ns"]
        || summary["stop_ns"] != background["stop_ns"]
    {
        return Err("8192原长负载/成功唯一key/真实TTL轮换不满足".into());
    }
    Ok(())
}

fn read_json(case: &Case, name: &str) -> Result<Value, String> {
    serde_json::from_slice(
        &std::fs::read(case.directory.join(name)).map_err(|e| format!("{name}：{e}"))?,
    )
    .map_err(|e| format!("{name}：{e}"))
}

/// 所有原进程完成收尾后派生；输入缺失也保存FAIL，不把4096原红报告改成PASS。
pub(super) fn save_acceptance(
    case: &Case,
    entries: usize,
    lifecycle: &Value,
) -> Result<Value, String> {
    let evaluated = (|| {
        let background = read_json(case, "background.json")?;
        let recovery = read_json(case, "recovery_writes.json")?;
        let final_stop = read_json(case, "final_stop.json")?;
        let summary = read_json(case, "load_summary.json")?;
        let attempts = std::fs::read_to_string(case.directory.join("attempts.jsonl"))
            .map_err(|e| e.to_string())?
            .lines()
            .map(serde_json::from_str)
            .collect::<Result<Vec<Value>, _>>()
            .map_err(|e| e.to_string())?;
        accept(
            u64::try_from(entries).map_err(|e| e.to_string())?,
            &Inputs {
                background: &background,
                recovery: recovery.as_array().ok_or("恢复写不是数组")?,
                final_stop: &final_stop,
                summary: &summary,
                lifecycle,
                attempts: &attempts,
                logs: &case.logs("gateway")?,
            },
        )
    })();
    let evidence = match evaluated {
        Ok(evidence) => evidence,
        Err(error) => {
            json!({"verdict":"FAIL","max_entries":entries,"error":error,"lifecycle":lifecycle})
        }
    };
    std::fs::write(
        case.directory.join("s48_acceptance.json"),
        serde_json::to_vec_pretty(&evidence).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    Ok(evidence)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(
        (id, key): (&str, &str),
        class: &str,
        at: u64,
        after: u64,
        removed: u64,
        decision: &str,
        entries: u64,
    ) -> Value {
        let mut matrix = [[0_u64; 10]; 4];
        matrix[3][7] = after;
        let capacity = decision == "Capacity";
        let bound = threshold(entries, class).unwrap();
        json!({"message":MARKER,"point":"post_gc_pre_admission","classes":CLASSES,"stages":STAGES,
            "max_entries":entries,"ttl_ns":TTL_NS,"now_ns":at,"method":"set_enabled","request_id":id,"key":key,
            "incoming_class":class,"before_gc_len":after+removed,"gc_removed":removed,"post_gc_len":after,
            "threshold":bound,"class_stage":serde_json::to_string(&matrix).unwrap(),"decision":decision,
            "entry_id":if capacity {"None"} else {"Some(EntryId(5000))"},
            "capacity_class":if capacity {format!("Some({class})")} else {"None".into()},
            "capacity_len":if capacity {format!("Some({after})")} else {"None".into()},
            "capacity_threshold":if capacity {format!("Some({bound})")} else {"None".into()}})
    }

    fn ack(id: &str, dry: bool) -> Value {
        json!({"ok":true,"kind":"ack","request_id":id,"dry_run":dry,"cleared_at_ns":7})
    }

    struct Fixture {
        background: Value,
        recovery: Vec<Value>,
        final_stop: Value,
        summary: Value,
        lifecycle: Value,
        attempts: Vec<Value>,
        snapshots: Vec<Value>,
    }
    impl Fixture {
        fn red() -> Self {
            let mut failed = json!({"sequence":1,"request_id":"r1","idempotency_key":"k1","started_ns":50_000_100_u64,
                "due_ns":50_000_100_u64,"finished_ns":50_000_200_u64,"ok":false,"terminal":true,
                "response":{"ok":false,"kind":"status","code":"ResourceExhausted","message":"idempotency cache full (3840/3840)"}});
            failed["deadline_ns"] = json!(2_050_000_100_u64);
            let first = json!({"stamp_ns":50_000_200_u64,"error":format!("请求 1 / r1：{}",failed["response"]),
                "source":{"kind":"request","sequence":1,"request_id":"r1","idempotency_key":"k1"}});
            let normal = json!({"sequence":0,"request_id":"r0","idempotency_key":"k0","started_ns":100,"due_ns":100,"finished_ns":200,
                "ok":true,"terminal":true,"response":ack("r0",true)});
            let lower = json!({"attempted":true,"ok":true,"kind":"ack","request_id":"lower","dry_run":false,"cleared_at_ns":7,
                "started_ns":800_000_000_u64,"finished_ns":800_000_100_u64,
                "request":{"request_id":"lower","idempotency_key":"kl","dry_run":false,"enabled":false}});
            Self {
                background: json!({"valid_cadence":true,"valid_responses":false,"admission_rejection":null,"rr_target_per_second":20,
                    "tracked_limit":64,"client_timeout_ns":2_000_000_000_u64,"drain_budget_ns":2_500_000_000_u64,"peak_tracked":1,
                    "launched":2,"completed":2,"unfinished":[],"started_ns":100,"stop_ns":50_000_200_u64,"drained_ns":50_000_300_u64,
                    "first_failure":first,"failures":[first],"rr_requests":[normal,failed]}),
                recovery: vec![],
                final_stop: json!({"ok":true,"get_state":{"ok":true,"safety_state":3},"disable":lower}),
                summary: json!({"successful_unique_count":1,"successful_unique_keys":["k0"],"trace_dropped":0,
                    "measurement_error":format!("背景负载首错：{first}")}),
                lifecycle: json!({"measurement_ok":false,"measurement_error":format!("背景负载首错：{first}"),
                    "final_stop_ok":true,"gateway_stop_ok":true,"s0_stop_ok":true}),
                attempts: vec![],
                snapshots: vec![
                    snapshot(
                        ("r1", "k1"),
                        "Normal",
                        50_000_150,
                        3840,
                        0,
                        "Capacity",
                        4096,
                    ),
                    snapshot(
                        ("lower", "kl"),
                        "Lower",
                        800_000_050,
                        3840,
                        0,
                        "Fresh",
                        4096,
                    ),
                ],
            }
        }
        fn evaluate(&self, entries: u64) -> Result<Value, String> {
            accept(
                entries,
                &Inputs {
                    background: &self.background,
                    recovery: &self.recovery,
                    final_stop: &self.final_stop,
                    summary: &self.summary,
                    lifecycle: &self.lifecycle,
                    attempts: &self.attempts,
                    logs: &self
                        .snapshots
                        .iter()
                        .map(Value::to_string)
                        .collect::<Vec<_>>()
                        .join("\n"),
                },
            )
        }

        fn green() -> Self {
            let mut f = Self::red();
            let rows: Vec<_> = (0_u64..8193)
                .map(|index| {
                    let start = 100 + index * RR_PERIOD_NS;
                    let id = format!("r{index}");
                    json!({"sequence":index,"request_id":id,"idempotency_key":format!("k{index}"),
                    "due_ns":start,"started_ns":start,"finished_ns":start+100,"ok":true,
                    "terminal":true,"response":ack(&id,true)})
                })
                .collect();
            let stop = number(rows.last().unwrap(), "finished_ns").unwrap();
            f.background["rr_requests"] = json!(rows);
            f.background["launched"] = json!(8193);
            f.background["completed"] = json!(8193);
            f.background["stop_ns"] = json!(stop);
            f.background["drained_ns"] = json!(stop + 100);
            f.background["first_failure"] = Value::Null;
            f.background["failures"] = json!([]);
            f.background["valid_responses"] = json!(true);
            f.lifecycle["measurement_ok"] = json!(true);
            f.lifecycle["measurement_error"] = Value::Null;
            let keys: BTreeSet<_> = (0..8193).map(|index| format!("k{index}")).collect();
            f.summary = json!({"successful_unique_count":8193,"successful_unique_keys":keys,"trace_dropped":0,
                "measurement_error":null,"minimum_ns":360_000_000_000_u64,"maximum_ns":600_000_000_000_u64,
                "started_ns":100,"stop_ns":stop});
            let lower_start = stop + 700_000_000;
            f.final_stop["disable"]["started_ns"] = json!(lower_start);
            f.final_stop["disable"]["finished_ns"] = json!(lower_start + 100);
            f.snapshots = vec![
                snapshot(
                    ("r8192", "k8192"),
                    "Normal",
                    stop - 50,
                    6000,
                    1,
                    "Fresh",
                    8192,
                ),
                snapshot(
                    ("lower", "kl"),
                    "Lower",
                    lower_start + 50,
                    6001,
                    0,
                    "Fresh",
                    8192,
                ),
            ];
            f
        }
    }

    #[test]
    fn actual_normal_capacity_accepts_concurrent_total_above_threshold() {
        let mut f = Fixture::red();
        assert_eq!(f.evaluate(4096).unwrap()["verdict"], "EXPECTED_RED");
        f.snapshots[0] = snapshot(
            ("r1", "k1"),
            "Normal",
            50_000_150,
            3841,
            0,
            "Capacity",
            4096,
        );
        f.background["rr_requests"][1]["response"]["message"] =
            json!("idempotency cache full (3841/3840)");
        f.background["first_failure"]["error"] = json!(format!(
            "请求 1 / r1：{}",
            f.background["rr_requests"][1]["response"]
        ));
        f.background["failures"][0] = f.background["first_failure"].clone();
        f.lifecycle["measurement_error"] =
            json!(format!("背景负载首错：{}", f.background["first_failure"]));
        f.summary["measurement_error"] = f.lifecycle["measurement_error"].clone();
        assert!(f.evaluate(4096).is_ok());
    }

    #[test]
    fn wrong_source_missing_duplicate_snapshot_and_request_identity_fail() {
        for field in ["key", "request_id", "incoming_class", "method"] {
            let mut f = Fixture::red();
            f.snapshots[0][field] = json!("wrong");
            assert!(f.evaluate(4096).is_err());
        }
        let mut f = Fixture::red();
        f.snapshots.remove(0);
        assert!(f.evaluate(4096).is_err());
        let mut f = Fixture::red();
        f.snapshots.push(f.snapshots[0].clone());
        assert!(f.evaluate(4096).is_err());
        for error in ["wire limit full", "gateway queue full"] {
            let mut f = Fixture::red();
            f.background["rr_requests"][1]["response"]["message"] = json!(error);
            assert!(f.evaluate(4096).is_err());
        }
        let mut f = Fixture::red();
        f.background["first_failure"]["source"]["kind"] = json!("supervisor");
        assert!(f.evaluate(4096).is_err());
    }

    #[test]
    fn post_capacity_join_drain_disarm_live_failure_and_wrong_ack_never_expected_red() {
        for message in ["背景任务 join 失败", "背景 drain 超过固定 2500ms"] {
            let mut f = Fixture::red();
            f.background["failures"]
                .as_array_mut()
                .unwrap()
                .push(json!({"stamp_ns":50_000_250,
                "source":{"kind":"supervisor"},"error":message}));
            assert!(f.evaluate(4096).is_err());
        }
        let mut f = Fixture::red();
        f.attempts
            .push(json!({"invalid":true,"disarm_ok":false,"dropped":0,
            "error":f.lifecycle["measurement_error"]}));
        assert!(f.evaluate(4096).is_err());
        for field in ["final_stop_ok", "gateway_stop_ok", "s0_stop_ok"] {
            let mut f = Fixture::red();
            f.lifecycle[field] = json!(false);
            assert!(f.evaluate(4096).is_err());
        }
        let mut f = Fixture::red();
        f.final_stop["disable"]["request_id"] = json!("wrong_ack");
        assert!(f.evaluate(4096).is_err());
        assert_eq!(f.final_stop["disable"]["request_id"], "wrong_ack");
        let mut f = Fixture::red();
        f.recovery.push(json!({"method":"ClearEStop","request_id":"r2","idempotency_key":"k2",
            "started_ns":300,"finished_ns":400,"ok":false,"response":{"ok":false,"kind":"timeout"}}));
        assert!(f.evaluate(4096).is_err());
    }

    #[test]
    fn matrix_gc_stage_threshold_and_final_lower_fresh_are_actual_facts() {
        for (field, value) in [
            ("gc_removed", json!(1)),
            ("class_stage", json!("[[1]]")),
            ("threshold", json!(4096)),
            ("point", json!("after_admission")),
            ("capacity_class", json!("Some(Lower)")),
        ] {
            let mut f = Fixture::red();
            f.snapshots[0][field] = value;
            assert!(f.evaluate(4096).is_err());
        }
        let mut f = Fixture::red();
        f.snapshots[1]["decision"] = json!("Replay");
        assert!(f.evaluate(4096).is_err());
        let mut f = Fixture::red();
        f.snapshots[1]["entry_id"] = json!("None");
        assert!(f.evaluate(4096).is_err());
    }

    #[test]
    fn all_original_requests_and_unique_keys_are_required() {
        let mut f = Fixture::red();
        f.background["launched"] = json!(3);
        f.background["completed"] = json!(3);
        assert!(f.evaluate(4096).is_err());
        let mut f = Fixture::red();
        f.background["rr_requests"][1]["idempotency_key"] = json!("k0");
        assert!(f.evaluate(4096).is_err());
        let mut f = Fixture::red();
        f.background["rr_requests"][0]["started_ns"] = json!(50_000_100_u64);
        assert!(f.evaluate(4096).is_err());
        let mut f = Fixture::red();
        f.background["drained_ns"] = json!(2_550_000_201_u64);
        assert!(f.evaluate(4096).is_err());
        let mut f = Fixture::red();
        f.summary["trace_dropped"] = json!(1);
        assert!(f.evaluate(4096).is_err());
    }

    #[test]
    fn green_requires_successful_measurement_gc_not_final_lower_gc() {
        let f = Fixture::green();
        assert_eq!(f.evaluate(8192).unwrap()["verdict"], "GREEN");
        let mut f = Fixture::green();
        f.snapshots.remove(0);
        f.snapshots[0]["gc_removed"] = json!(1);
        f.snapshots[0]["before_gc_len"] = json!(6002);
        assert!(
            f.evaluate(8192).is_err(),
            "只有Lower回收不证明测量期TTL轮换"
        );
        let mut f = Fixture::green();
        f.snapshots[0] = snapshot(("r0", "k0"), "Normal", 150, 1, 1, "Fresh", 8192);
        assert!(
            f.evaluate(8192).is_err(),
            "负载开始不足300s不能冒本轮TTL释放"
        );
        let mut f = Fixture::green();
        f.background["rr_requests"][0]["ok"] = json!(false);
        f.summary["successful_unique_count"] = json!(8192);
        f.summary["successful_unique_keys"]
            .as_array_mut()
            .unwrap()
            .retain(|key| key != "k0");
        assert!(
            f.evaluate(8192).is_err(),
            "valid_responses标签不能覆盖原失败行"
        );
    }

    #[test]
    fn green_duration_and_unique_count_keep_original_strict_edges() {
        for (elapsed, count, pass) in [
            (359_999_999_999_u64, 8193, false),
            (360_000_000_000, 8193, true),
            (600_000_000_000, 8193, true),
            (600_000_000_001, 8193, false),
            (400_000_000_000, 8192, false),
        ] {
            let mut f = Fixture::green();
            f.background["stop_ns"] = json!(100 + elapsed);
            f.summary["stop_ns"] = json!(100 + elapsed);
            assert_eq!(
                green(&f.background, &f.summary, &f.lifecycle, 1, count).is_ok(),
                pass
            );
        }
    }
}
