//! 三代原目录链：运行期 Prepare 中断 → 启动恢复中断 → 无配置完成恢复。

mod record;

use super::{
    Args, Case, DAEMON_LOG, ExpectedExit, Owned, Point, Report, STARTUP_NS, combine,
    first_invocation, hash_tree, original_child_pid, persistent_snapshot, reap_owned,
    record_child_identity, restart_same_directory, save_bytes, save_json, verify_killed_files,
    verify_restarted_files, verify_temporary_files, verify_trust_lock, within_deadline,
};
use record::Record;
use robot_core::time::{RobotClock, SystemClock};
use robot_s0::comm_fault_observation::ObservationSnapshot;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::process::Child;

const FILTER: &str = "R4-T8-RECOVERY-KILL";
const NAME: &str = "recovery_primary_rename_interrupts_before_workers_and_third_generation_repairs";

pub(super) async fn run(args: &Args, report: &mut Report) -> Result<(), String> {
    if !cfg!(target_os = "linux") || args.case.as_deref() != Some(FILTER) {
        return Err("R4b4只接受Linux mock固定单例".into());
    }
    let count = report.cases.len();
    let started = SystemClock.now().0;
    report.record(NAME, FILTER, started, exercise(args).await);
    if report.cases.len().checked_sub(count) != Some(1) {
        return Err("R4b4未实际登记唯一三代例".into());
    }
    Ok(())
}

fn generation(shared: &Case, name: &str) -> Result<Case, String> {
    let case = Case {
        directory: shared.directory.join("invocations").join(name),
        config: shared.config.clone(),
        robot_id: shared.robot_id.clone(),
        latch: shared.latch.clone(),
        trust: shared.trust.clone(),
        bin_dir: shared.bin_dir.clone(),
    };
    std::fs::create_dir(&case.directory).map_err(|error| error.to_string())?;
    Ok(case)
}

fn before_workers(observation: &ObservationSnapshot, base: u64) -> Result<Value, String> {
    if !observation.observation_ready
        || observation.preinit_ns == 0
        || observation.main_start_ns != base
        || base == 0
        || observation.spawned_ns != 0
        || observation.ready_ns != 0
        || observation.ready_reached
        || observation.runtime_armed_ns != 0
        || observation.worker_bitmap != 0
        || observation.first_io_ns != 0
        || observation.serial_calls != [0; 6]
        || observation.hook_ready
        || observation.fired_ns != 0
        || observation.supervisor_exit_ns != 0
    {
        return Err(format!("第二代未停在原恢复/CP1域：{observation:?}"));
    }
    Ok(
        json!({"main_start_ns":observation.main_start_ns,"preinit_ns":observation.preinit_ns,"observation_ready":observation.observation_ready,
        "spawned_ns":observation.spawned_ns,"ready_ns":observation.ready_ns,"ready_reached":observation.ready_reached,
        "runtime_armed_ns":observation.runtime_armed_ns,"worker_bitmap":observation.worker_bitmap,"first_io_ns":observation.first_io_ns,
        "serial_calls":observation.serial_calls,"hook_ready":observation.hook_ready,"fired_ns":observation.fired_ns,"supervisor_exit_ns":observation.supervisor_exit_ns}),
    )
}

fn identity(record: &Record, original: &Value, chain: &Value, pid: u32) -> Result<(), String> {
    let directory = &original["directory"];
    let marker = &original["files"]["estop_latch.clear_pending"];
    if record.pid != pid
        || record.dir_dev != directory["dev"].as_u64().ok_or("原目录dev缺失")?
        || record.dir_ino != directory["ino"].as_u64().ok_or("原目录ino缺失")?
        || record.marker_dev != marker["dev"].as_u64().ok_or("原marker dev缺失")?
        || record.marker_ino != marker["ino"].as_u64().ok_or("原marker ino缺失")?
        || marker["bytes"] != json!(record.bytes.as_slice())
        || marker["len"] != 112
        || chain["txn_id"].as_str() != Some(record.txn.to_string().as_str())
        || chain["expected_epoch"].as_u64() != Some(record.epoch)
        || !record.latched
    {
        return Err("第二代恢复记录不是第一代同原目录/marker/完整txn".into());
    }
    Ok(())
}

fn intermediate_files(original: &Value, current: &Value, pids: (u32, u32)) -> Result<(), String> {
    verify_trust_lock(original, current, pids)?;
    for field in ["config", "trust"] {
        if original[field] != current[field] {
            return Err(format!("第二代替换共享{field}"));
        }
    }
    for field in ["directory", "lock"] {
        for key in ["present", "dev", "ino"] {
            if original[field][key] != current[field][key] {
                return Err(format!("第二代改变原{field}/{key}"));
            }
        }
    }
    if original["files"]["estop_latch.clear_pending"]
        != current["files"]["estop_latch.clear_pending"]
    {
        return Err("第二代改变原marker身份或规范112字节".into());
    }
    let files = current["files"].as_object().ok_or("第二代文件集合缺失")?;
    if files.len() != 3
        || files["estop_latch"]["bytes"] != json!(b"1\n".as_slice())
        || files["estop_latch.bak"]["bytes"] != json!(b"0\n".as_slice())
        || original["files"]["estop_latch.bak"] != files["estop_latch.bak"]
    {
        return Err("第二代不是原RenamePrimary后的1/0与未触碰backup".into());
    }
    Ok(())
}

fn no_startup_finish(case: &Case) -> Result<(), String> {
    let logs = case.logs("s0")?;
    if logs.contains("R4_RECOVERY_DIAGNOSTIC_V1")
        || logs.contains("initial estop_latched")
        || logs.contains("boot_ready")
    {
        return Err("第二代已完成启动或出现恢复观测独立失败".into());
    }
    let trace = case.trace("s0")?;
    if trace.armed_seq() != 0
        || trace.dropped() != 0
        || !trace
            .drain(robot_s0::comm_fault_trace::TRACE_CAPACITY)
            .is_empty()
    {
        return Err("第二代产生了运行期采样或事件".into());
    }
    Ok(())
}

async fn observe_and_kill(
    owned: &mut Owned,
    case: &Case,
    original: &Value,
    chain: &Value,
    first_pid: u32,
    started: u64,
) -> Result<Value, String> {
    let until = started
        .checked_add(STARTUP_NS)
        .ok_or("第二代父启动包络溢出")?;
    let record = loop {
        owned.running(until, "第二代唯一完整恢复记录")?;
        let stdout = std::fs::read(case.directory.join("s0.stdout.log"))
            .map_err(|error| error.to_string())?;
        if let Some(record) = record::extract(&stdout)? {
            break record;
        }
        no_startup_finish(case)?;
        let observation = case.observation("s0")?;
        if observation.main_start_ns != 0 {
            within_deadline(
                SystemClock.now().0,
                observation
                    .main_start_ns
                    .saturating_add(robot_s0::boot_supervisor::PRE_BOOT_BUDGET.0),
                "原CP1恢复记录",
            )?;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    };
    let pid = owned
        .child
        .as_ref()
        .and_then(Child::id)
        .ok_or("第二代活Child无原PID")?;
    identity(&record, original, chain, pid)?;
    let observation = before_workers(&case.observation("s0")?, record.base)?;
    no_startup_finish(case)?;
    let current = persistent_snapshot(case)?;
    intermediate_files(original, &current, (first_pid, pid))?;
    let accepted_at = SystemClock.now().0;
    record.window(accepted_at, accepted_at)?;
    save_bytes(
        &case.directory.join("reached_record.txt"),
        record.raw.as_bytes(),
    )?;
    save_json(
        &case.directory.join("persistent_before_signal.json"),
        &current,
    )?;
    owned.running(record.pause_until, "第二代信号前原Child")?;
    let current_observation = before_workers(&case.observation("s0")?, record.base)?;
    no_startup_finish(case)?;
    let before = SystemClock.now().0;
    record.window(before, before)?;
    let raw_pid = i32::try_from(pid).map_err(|error| error.to_string())?;
    // SAFETY：PID刚从本代仍存活的原Child读取；不会向进程组或其他对象发信号。
    let result = unsafe { libc::kill(raw_pid, libc::SIGKILL) };
    let errno = if result == 0 {
        None
    } else {
        std::io::Error::last_os_error().raw_os_error()
    };
    let after = SystemClock.now().0;
    owned.kill_sent = result == 0;
    owned.kill_after_ns = (result == 0).then_some(after);
    let kill = json!({"pid":pid,"signal":libc::SIGKILL,"return":result,"errno":errno,"before_ns":before,"after_ns":after,
        "accepted_at_ns":accepted_at,"main_start_ns":record.base,"cp1_deadline_ns":record.cp1()?,"reached_at_ns":record.reached,"pause_until_ns":record.pause_until});
    save_json(&case.directory.join("kill.json"), &kill)?;
    if result != 0 {
        return Err(format!("第二代原SIGKILL失败：{errno:?}"));
    }
    record.window(before, after)?;
    Ok(
        json!({"record":record.json(),"kill":kill,"observation_at_accept":observation,"observation_before_signal":current_observation}),
    )
}

async fn second_generation(
    case: &Case,
    original: &Value,
    chain: &Value,
    first_pid: u32,
) -> Result<Value, String> {
    let mut owned = Owned::default();
    let result:Result<Value,String>=async {
        save_json(&case.directory.join("invocation.json"),&json!({"generation":2,"configuration":"repair-rename-primary","recovery_hook":true,
            "runtime_file_kill":false,"writer_rr_node_capture_created":false,"config":case.config,"latch":case.latch,"trust":case.trust}))?;
        let mut command=case.command();command.env("ROBOT_TEST_W4_RECOVERY_KILL","repair-rename-primary")
            .env("ROBOT_LOG_FORMAT","json").env("RUST_LOG",DAEMON_LOG);
        let started=SystemClock.now().0;
        owned.child=Some(case.spawn("s0",command)?);record_child_identity(case,&owned)?;
        observe_and_kill(&mut owned,case,original,chain,first_pid,started).await
    }.await;
    let cleanup = reap_owned(&mut owned, case, ExpectedExit::Killed).await;
    let result = combine(result, cleanup.map(|_| ())).and_then(|evidence| {
        let stdout = std::fs::read(case.directory.join("s0.stdout.log"))
            .map_err(|error| error.to_string())?;
        let final_record = record::extract(&stdout)?.ok_or("第二代真reap后缺唯一完整原记录")?;
        if final_record.json() != evidence["record"] {
            return Err("第二代真reap后原恢复记录变化或重复".into());
        }
        no_startup_finish(case)?;
        let _ = before_workers(&case.observation("s0")?, final_record.base)?;
        Ok(evidence)
    });
    if let Err(error) = &result {
        let _ = save_json(
            &case.directory.join("failure.json"),
            &json!({"error":error,"at_ns":SystemClock.now().0}),
        );
    }
    result
}

fn freeze(case: &Case, shared: &Case, label: &str) -> Result<BTreeMap<String, String>, String> {
    let manifest = hash_tree(&case.directory)?;
    save_json(
        &shared.directory.join(format!("{label}_manifest.json")),
        &json!(manifest),
    )?;
    Ok(manifest)
}

fn unchanged(
    case: &Case,
    shared: &Case,
    label: &str,
    before: &BTreeMap<String, String>,
) -> Result<(), String> {
    let current = hash_tree(&case.directory)?;
    save_json(
        &shared
            .directory
            .join(format!("{label}_manifest_after.json")),
        &json!(current),
    )?;
    if &current != before {
        return Err(format!("后代改写{label}原证据集合/字节"));
    }
    Ok(())
}

async fn exercise(args: &Args) -> Result<Value, String> {
    let shared = Case::new(&args.evidence_dir, &args.bin_dir, NAME, 1)?;
    for path in [&shared.latch, &shared.directory.join("estop_latch.bak")] {
        std::fs::write(path, b"1\n").map_err(|error| error.to_string())?;
    }
    std::fs::create_dir(shared.directory.join("invocations")).map_err(|error| error.to_string())?;
    let first = generation(&shared, "1_prepare")?;
    let second = generation(&shared, "2_recovery")?;
    let third = generation(&shared, "3_complete")?;
    let first_result = first_invocation(&first, Point::PrepareSync).await;
    let first_manifest = freeze(&first, &shared, "first");
    let chain = combine(
        first_result,
        first_manifest.as_ref().map(|_| ()).map_err(Clone::clone),
    )?;
    let first_manifest = first_manifest?;
    verify_killed_files(&shared, Point::PrepareSync, &chain)?;
    let first_snapshot = persistent_snapshot(&shared)?;
    let first_pid = original_child_pid(&first)?;
    let _ = verify_temporary_files(Point::PrepareSync, &first_snapshot, first_pid)?;
    save_json(
        &shared.directory.join("persistent_after_first.json"),
        &first_snapshot,
    )?;
    let second_result = second_generation(&second, &first_snapshot, &chain, first_pid).await;
    let second_post = (|| {
        let manifest = freeze(&second, &shared, "second")?;
        unchanged(&first, &shared, "first_after_second", &first_manifest)?;
        let snapshot = persistent_snapshot(&shared)?;
        save_json(
            &shared.directory.join("persistent_after_second.json"),
            &snapshot,
        )?;
        let pid = original_child_pid(&second)?;
        intermediate_files(&first_snapshot, &snapshot, (first_pid, pid))?;
        Ok::<_, String>((manifest, snapshot, pid))
    })();
    let second_result = combine(
        second_result,
        second_post.as_ref().map(|_| ()).map_err(Clone::clone),
    )?;
    let (second_manifest, after_second, second_pid) = second_post?;
    let third_result = restart_same_directory(&third, Point::PrepareSync).await;
    let third_post = (|| {
        let snapshot = persistent_snapshot(&shared)?;
        save_json(
            &shared.directory.join("persistent_after_third.json"),
            &snapshot,
        )?;
        verify_restarted_files(
            &shared,
            Point::PrepareSync,
            &after_second,
            &snapshot,
            (second_pid, original_child_pid(&third)?),
        )?;
        let first_unchanged = unchanged(&first, &shared, "first_after_third", &first_manifest);
        combine(
            first_unchanged,
            unchanged(&second, &shared, "second_after_third", &second_manifest),
        )
    })();
    let third_result = combine(third_result, third_post)?;
    Ok(
        json!({"directory":shared.directory,"first":chain,"second":second_result,"third":third_result,
        "first_pid":first_pid,"second_pid":second_pid,"third_pid":original_child_pid(&third)?,"generations":3,
        "first_rr_retired":false,"previous_evidence_unchanged":true,"same_kernel_process_interruption_only":true}),
    )
}

#[cfg(test)]
mod tests;
