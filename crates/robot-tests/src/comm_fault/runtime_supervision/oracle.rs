//! 判据只比较原页/原输出；合成页测试不作为进程证据。
use super::{Case, Mode, ObservationSnapshot, Value, json};
use robot_s0::clear_estop_transaction::ClearTxnStage;
use robot_s0::latch_writer::CandidateKind;
use robot_s0::shutdown_mechanism::HandleKind;

pub(super) fn snapshot(p: &ObservationSnapshot) -> Value {
    let s = p.supervision;
    let view = |v: robot_s0::comm_fault_observation::supervision::ObligationObservation| json!({"at_ns":v.at_ns,"flags":v.flags,"since_ns":v.since_ns});
    let mut v = super::super::supervision::snapshot(p);
    v["main_start_ns"] = json!(p.main_start_ns);
    v["command_applied"] = json!(p.command_applied);
    v["hook_ready"] = json!(p.hook_ready);
    v["sup"] = json!({"mode":s.mode,"command_txn":s.command_txn.to_string(),"command_reached_ns":s.command_reached_ns,"announce_ns":s.announce_ns,"idle_entry_ns":s.idle_entry_ns,"obligation_id":s.obligation_id,"pending":view(s.pending),"active":view(s.active),"step_stage":s.step_stage,"step_entry_ns":s.step_entry_ns,"reached_ns":s.reached_ns,"main_hold_ns":s.main_hold_ns,"main_writer_observed_ns":s.main_writer_observed_ns,"exited":view(s.exited),"fallback_tag":s.fallback_tag,"fallback_settle":s.fallback_settle,"fallback":view(s.fallback),"pause_failure":s.pause_failure,"helper_owned":s.helper_owned,"helper_entered":s.helper_entered,"health_entered_ns":s.health_entered_ns,"systemd_entered_ns":s.systemd_entered_ns});
    v
}

pub(super) fn ready(mode: Mode, p: &ObservationSnapshot) -> Result<(), String> {
    if p.version != 4
        || p.main_start_ns == 0
        || !p.ready_reached
        || p.runtime_armed_ns < p.ready_ns
        || p.ready_ns == 0
        || p.supervisor_exit_ns != 0
        || p.supervision.mode != u64::from(mode as u8)
        || p.worker_bitmap != 0xff
        || p.supervision.pause_failure != 0
    {
        return Err(format!("原READY/RuntimeArmed接力不符：{p:?}"));
    }
    if mode.pre_ready() {
        let s = p.supervision;
        if p.command_applied != 1
            || !p.hook_ready
            || s.obligation_id == 0
            || s.pending.flags != 1
            || s.pending.since_ns < p.main_start_ns
            || s.pending.at_ns < s.pending.since_ns
            || s.pending.at_ns >= p.ready_ns
            || s.reached_ns >= p.ready_ns
            || s.reached_ns == 0
        {
            return Err("原history Pending/READY顺序不符".into());
        }
        if mode == Mode::PrePickup && (s.active.at_ns != 0 || s.idle_entry_ns == 0) {
            return Err("领取前暂停出现伪Active或无Idle源".into());
        }
        if mode == Mode::PreStep
            && (s.active.flags != 2
                || s.active.since_ns != s.pending.since_ns
                || s.active.at_ns >= p.ready_ns
                || s.step_stage != ClearTxnStage::CreatePrimary as u64
                || s.step_entry_ns >= p.ready_ns)
        {
            return Err("原READY前Claim/FileStep不符".into());
        }
    }
    Ok(())
}

fn fired(
    p: &ObservationSnapshot,
    reason: u64,
    kind: CandidateKind,
    deadline: u64,
) -> Result<(), String> {
    if p.fired_checkpoint != 3
        || p.fired_reason != reason
        || p.fired_deadline_ns != deadline
        || p.fired_candidates_bitmap != 1 << kind as u8
        || p.fired_ns < deadline
        || p.fired_ns > deadline + 250_000_000
        || p.shutdown_deadline_ns != 0
        || p.before_process_exit_ns != 0
        || p.supervision.pause_failure != 0
    {
        return Err(format!(
            "原唯一候选/250ms窗口或D状态不符：expected={deadline} page={p:?}"
        ));
    }
    Ok(())
}

fn original_exit(logs: &str) -> Result<Value, String> {
    if logs.contains("R4C_DIAGNOSTIC_V1") || logs.contains("R4C_NODE_CONFIG_WRITE_FAILED") {
        return Err("原诊断写出失败".into());
    }
    let lines = logs
        .lines()
        .filter_map(|line| line.strip_prefix("R4C_EXIT_V1 "))
        .collect::<Vec<_>>();
    if lines.len() != 1
        || logs
            .lines()
            .filter(|line| line.contains("R4C_EXIT_V1"))
            .count()
            != 1
    {
        return Err("原退出记录缺失/重复/交错".into());
    }
    let notify = logs
        .lines()
        .filter(|line| line.contains("R4C_NOTIFY_STOPPING_V1"))
        .collect::<Vec<_>>();
    if notify != ["R4C_NOTIFY_STOPPING_V1 mode=unset result=Ok(())"] {
        return Err("standalone原唯一notify必须是真unset跳过".into());
    }
    serde_json::from_str(lines[0]).map_err(|e| e.to_string())
}
fn joined(facts: &Value, kind: &str, ok: bool) -> Result<(), String> {
    let entries = facts["joined"]
        .as_array()
        .ok_or("缺原joined")?
        .iter()
        .filter(|v| v["kind"] == kind)
        .collect::<Vec<_>>();
    if entries.len() != 1 || entries[0]["ok"] != ok {
        return Err(format!("原{kind} join不符：{entries:?}"));
    }
    Ok(())
}

#[allow(clippy::too_many_lines)] // 固定七种场景的出口判据在同一match中保持可逐项核对。
pub(super) fn exit(
    mode: Mode,
    code: i32,
    p: &ObservationSnapshot,
    logs: &str,
) -> Result<Value, String> {
    if code != mode.expected_code() {
        return Err(format!("原退出码不符：{mode:?} code={code}"));
    }
    let s = p.supervision;
    if s.pause_failure != 0 {
        return Err(format!("有限注入边界失败：{}", s.pause_failure));
    }
    match mode {
        Mode::Command => {
            if s.command_txn == 0
                || s.command_reached_ns == 0
                || s.announce_ns < p.runtime_armed_ns
                || s.announce_ns > s.command_reached_ns
                || s.command_reached_ns > s.reached_ns
                || s.pending.at_ns != 0
            {
                return Err("原command/公告领取前事实不符".into());
            }
            fired(
                p,
                2,
                CandidateKind::Pickup,
                s.announce_ns.max(s.idle_entry_ns).max(p.runtime_armed_ns) + 500_000_000,
            )?;
        }
        Mode::PrePickup => {
            ready(mode, p)?;
            fired(
                p,
                2,
                CandidateKind::Pickup,
                s.pending
                    .since_ns
                    .max(s.idle_entry_ns)
                    .max(p.runtime_armed_ns)
                    + 500_000_000,
            )?;
        }
        Mode::PreStep => {
            ready(mode, p)?;
            fired(
                p,
                1,
                CandidateKind::FileStep,
                s.step_entry_ns.max(p.runtime_armed_ns) + 2_000_000_000,
            )?;
        }
        Mode::MainHeld => {
            writer_views(p)?;
            if s.main_hold_ns < p.runtime_armed_ns
                || s.pending.since_ns < s.main_hold_ns
                || s.main_writer_observed_ns != 0
                || p.writer_exited_at_ns < s.active.at_ns
                || s.exited.at_ns < p.writer_exited_at_ns
                || s.fallback_tag != 0
                || p.writer_joined_observed_at_ns != 0
            {
                return Err("原main暂停/Exited无D链不符".into());
            }
            fired(
                p,
                4,
                CandidateKind::WriterExited,
                p.writer_exited_at_ns.max(p.runtime_armed_ns) + 500_000_000,
            )?;
        }
        Mode::PanicBefore | Mode::PanicAfter => {
            writer_views(p)?;
            let d = p.shutdown_deadline_ns;
            let expected = if mode == Mode::PanicBefore {
                ClearTxnStage::CreatePrimary
            } else {
                ClearTxnStage::SyncBackupDirectory
            };
            if s.step_stage != expected as u64
                || s.main_writer_observed_ns < s.exited.at_ns
                || s.main_writer_observed_ns < p.writer_exited_at_ns
                || d < 10_000_000_000
                || d - 10_000_000_000 < s.main_writer_observed_ns
                || p.writer_joined_observed_at_ns < s.main_writer_observed_ns
                || p.before_process_exit_ns < p.writer_joined_observed_at_ns
                || p.before_process_exit_ns >= d
                || p.fired_checkpoint != 0
            {
                return Err("原panic→main→D→真join→原出口链不符".into());
            }
            let facts = original_exit(logs)?;
            joined(&facts, "LatchWriter", false)?;
            if facts["writer_join"] != "panicked"
                || facts["residual"] != p.exit_residual_bitmap
                || facts["fallback_tag"] != s.fallback_tag
                || facts["fallback_settle"] != s.fallback_settle
                || s.fallback_tag != 4
                || s.fallback_settle != 1
                || s.fallback.flags != 0
                || s.fallback.at_ns < p.writer_joined_observed_at_ns
            {
                return Err(format!("原唯一备用写结果不符：{facts}"));
            }
            if logs
                .lines()
                .filter(|line| line.contains("备用文件写完成，退出仍非 clean"))
                .count()
                != 1
            {
                return Err("原补写成功记录不是一次".into());
            }
            return Ok(facts);
        }
        Mode::Health => {
            let facts = original_exit(logs)?;
            joined(&facts, "HealthMonitor", false)?;
            joined(&facts, "LatchWriter", true)?;
            if s.helper_owned & 1 == 0
                || s.helper_entered != 1
                || s.health_entered_ns < p.ready_ns
                || p.exit_residual_bitmap & u64::from(HandleKind::HealthMonitor.bit()) != 0
                || p.fired_checkpoint != 0
                || p.before_process_exit_ns == 0
                || p.before_process_exit_ns >= p.shutdown_deadline_ns
                || facts["failures"] != json!(["Panicked(HealthMonitor)"])
            {
                return Err(format!(
                    "原health真进入/panic/join/失败集合不符：{facts} page={p:?}"
                ));
            }
            return Ok(facts);
        }
    }
    Ok(
        json!({"reason":p.fired_reason,"deadline_ns":p.fired_deadline_ns,"fired_ns":p.fired_ns,"candidates":p.fired_candidates_bitmap}),
    )
}

fn writer_views(p: &ObservationSnapshot) -> Result<(), String> {
    let s = p.supervision;
    if s.obligation_id == 0
        || s.pending.flags != 1
        || s.active.flags != 2
        || s.exited.flags != 1
        || s.pending.since_ns < p.runtime_armed_ns
        || s.pending.at_ns < s.pending.since_ns
        || s.active.at_ns < s.pending.at_ns
        || s.exited.at_ns < s.active.at_ns
        || s.active.since_ns != s.pending.since_ns
        || s.exited.since_ns != s.pending.since_ns
        || p.command_applied != 1
    {
        return Err(format!("原Pending/Active/panic恢复start不符：{s:?}"));
    }
    Ok(())
}

pub(super) fn files(case: &Case, mode: Mode) -> Result<Value, String> {
    let names = ["estop_latch", "estop_latch.bak"];
    let expected = if matches!(mode, Mode::Command | Mode::PanicBefore | Mode::PanicAfter) {
        b"1\n"
    } else {
        b"0\n"
    };
    let mut values = Vec::new();
    for name in names {
        let path = case.directory.join(name);
        let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
        if bytes != expected {
            return Err(format!("原{mode:?}文件{name}={bytes:?}"));
        }
        values.push(
            json!({"name":name,"bytes":bytes,"sha256":super::super::report::file_hash(&path)?}),
        );
    }
    if case.directory.join("estop_latch.clear_pending").exists() {
        return Err("义务/领取前场景不应创建Clear marker".into());
    }
    Ok(json!(values))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn original_exit_line_requires_one_uninterleaved_record_and_no_output_failure() {
        assert!(
            original_exit("R4C_EXIT_V1 {}\nR4C_NOTIFY_STOPPING_V1 mode=unset result=Ok(())\n")
                .is_ok()
        );
        for logs in [
            "",
            "R4C_EXIT_V1 {}\nR4C_EXIT_V1 {}\n",
            "prefix R4C_EXIT_V1 {}\n",
            "R4C_EXIT_V1 {}\nR4C_DIAGNOSTIC_V1 failed",
        ] {
            assert!(original_exit(logs).is_err());
        }
    }
    #[test]
    fn joined_fact_rejects_missing_duplicate_and_wrong_original_result() {
        let good = json!({"joined":[{"kind":"LatchWriter","ok":false}]});
        assert!(joined(&good, "LatchWriter", false).is_ok());
        assert!(joined(&good, "LatchWriter", true).is_err());
        assert!(joined(&json!({"joined":[]}), "LatchWriter", false).is_err());
        assert!(joined(&json!({"joined":[{"kind":"LatchWriter","ok":false},{"kind":"LatchWriter","ok":false}]}),"LatchWriter",false).is_err());
    }
}
