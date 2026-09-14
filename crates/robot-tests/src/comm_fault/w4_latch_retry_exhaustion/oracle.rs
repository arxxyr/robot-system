//! R4b2 有限判据：解析原观察，不制造权限、领取回执或真实执行证据。

use robot_s0::clear_estop_transaction::{ClearTxnStage as S, StepPhase};
use robot_s0::comm_fault_trace::w4::{CommandKind as C, CompletionKind as K, TokenKind as T};
use robot_s0::latch_writer::ClearTxnIdentity;
use robot_s0::shutdown_mechanism::{HandleKind, ShutdownFailure};
use serde_json::{Value, json};

use super::super::w4_latch_crash::{CommandFact, command_evidence};

const IPC: &str = "R4重投原责任";
const RR: &str = "R4原ClearRR责任";
const EXIT: &str = "R4运行期原退出结果";
const PREPARE: [S; 15] = [
    S::CreateMarker,
    S::SampleMarkerIdentity,
    S::WriteMarker,
    S::SyncMarkerFile,
    S::SyncMarkerDirectory,
    S::CreateBackup,
    S::WriteBackup,
    S::SyncBackupFile,
    S::RenameBackup,
    S::SyncBackupDirectory,
    S::CreatePrimary,
    S::WritePrimary,
    S::SyncPrimaryFile,
    S::RenamePrimary,
    S::SyncPrimaryDirectory,
];
const REMOVE: [S; 4] = [
    S::ReadMarker,
    S::ValidateMarker,
    S::RemoveMarker,
    S::SyncRemovalDirectory,
];

pub(super) fn validate_chain(commands: &[CommandFact]) -> Result<Value, String> {
    if commands.len() != 11 {
        return Err("耗尽链必须恰为11个原scope，禁止缺失或source12".into());
    }
    let txn = commands[0].txn;
    if txn.txn_id == 0 || !txn.original_latched {
        return Err("原事务身份缺失或不是latched事务".into());
    }
    for (index, command) in commands.iter().enumerate() {
        let (kind, before, after, completion, stages, slot): (_, _, _, _, &[S], _) = match index {
            0 => (
                C::Prepare,
                T::None,
                T::Prepared,
                K::Prepared,
                &PREPARE,
                None,
            ),
            1 => (
                C::RemoveCommitted,
                T::Prepared,
                T::Unsynced,
                K::RemoveFailedUnsynced,
                &REMOVE,
                Some(0),
            ),
            _ => (
                C::Resync,
                T::Unsynced,
                T::Unsynced,
                K::RemoveFailedUnsynced,
                &[S::SyncRemovalDirectory],
                Some(1),
            ),
        };
        if command.source != u64::try_from(index).map_err(|error| error.to_string())? + 1
            || command.txn != txn
            || (
                command.command,
                command.before,
                command.after,
                command.completion,
            ) != (kind, before, after, completion)
            || !command
                .steps
                .iter()
                .map(|step| step.stage)
                .eq(stages.iter().copied())
            || command.failure != slot.map(|_| (S::SyncRemovalDirectory, StepPhase::Operation))
        {
            return Err(format!("原事务/令牌/完整步骤或completion不符：{command:?}"));
        }
        for step in &command.steps {
            let expected_slot = match step.stage {
                S::SyncRemovalDirectory => slot,
                _ => None,
            };
            if step.slot != expected_slot || step.succeeded != expected_slot.is_none() {
                return Err(format!("原操作槽或返回值不符：{step:?}"));
            }
        }
    }
    Ok(
        json!({"txn_id":txn.txn_id.to_string(),"expected_epoch":txn.expected_epoch,
        "original_latched":txn.original_latched,"scope_count":11,"alias_errors":10,
        "commands":command_evidence(commands)}),
    )
}

fn records(logs: &str, marker: &str) -> Result<Vec<Value>, String> {
    let mut result = Vec::new();
    for line in logs.lines().filter(|line| line.contains(marker)) {
        let record: Value = serde_json::from_str(line)
            .map_err(|error| format!("原{marker}日志不是完整JSON：{error}"))?;
        if text(&record, "message")? != marker {
            return Err(format!("原日志marker不在message：{marker}"));
        }
        if result.len() >= 64 {
            return Err(format!("原{marker}状态变化日志超过固定64条边界"));
        }
        result.push(record);
    }
    Ok(result)
}

fn text<'a>(record: &'a Value, key: &str) -> Result<&'a str, String> {
    record
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("原日志字段{key}缺失或不是字符串"))
}
fn number(record: &Value, key: &str) -> Result<u64, String> {
    record
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("原日志字段{key}缺失或不是u64"))
}
fn flag(record: &Value, key: &str) -> Result<bool, String> {
    record
        .get(key)
        .and_then(Value::as_bool)
        .ok_or_else(|| format!("原日志字段{key}缺失或不是bool"))
}
fn equal_text(record: &Value, key: &str, expected: &str) -> Result<(), String> {
    let actual = text(record, key)?;
    if actual != expected {
        return Err(format!("原{key}应为{expected}，实际{actual}"));
    }
    Ok(())
}
fn some_number(record: &Value, key: &str) -> Result<u64, String> {
    let raw = text(record, key)?;
    raw.strip_prefix("Some(")
        .and_then(|raw| raw.strip_suffix(')'))
        .ok_or_else(|| format!("原{key}缺Some实值"))?
        .parse()
        .map_err(|error| format!("原{key}不是Some(u64)：{error}"))
}

struct Identity {
    txn: ClearTxnIdentity,
    route: String,
    logical: u64,
    owner_domain: String,
    owner_epoch: u64,
    sampled_epoch: u64,
}
impl Identity {
    fn read(first: &Value) -> Result<Self, String> {
        let txn_id = text(first, "txn_id")?
            .parse::<u128>()
            .map_err(|error| error.to_string())?;
        let identity = Self {
            txn: ClearTxnIdentity {
                txn_id,
                expected_epoch: number(first, "expected_epoch")?,
                original_latched: flag(first, "original_latched")?,
            },
            route: text(first, "route")?.to_owned(),
            logical: some_number(first, "logical_committed_at")?,
            owner_domain: text(first, "owner_domain")?.to_owned(),
            owner_epoch: some_number(first, "owner_epoch")?,
            sampled_epoch: some_number(first, "sampled_epoch")?,
        };
        if txn_id == 0
            || !identity.txn.original_latched
            || identity.logical == 0
            || !identity.route.starts_with("Some(")
            || !identity.owner_domain.starts_with("Some(")
        {
            return Err("原txn/route/owner或逻辑提交实值缺失".into());
        }
        Ok(identity)
    }

    fn validate(&self, row: &Value, request_id: &str, deadline: u64) -> Result<(), String> {
        for (key, expected) in [
            ("txn_id", self.txn.txn_id.to_string()),
            ("route", self.route.clone()),
            ("request_id", request_id.to_owned()),
            ("before_txn", format!("Some({:?})", self.txn)),
            ("cleanup_kind", "Some(Finalize)".into()),
            ("owner_kind", "Finalizing".into()),
            ("owner_domain", self.owner_domain.clone()),
            ("owner_original_latched", "Some(true)".into()),
            ("physical_done_at", "None".into()),
            ("next", "None".into()),
            (
                "locked",
                format!(
                    "Some(Cleared {{ cleared_at_ns: {}, superseded: false }})",
                    self.logical
                ),
            ),
        ] {
            equal_text(row, key, &expected)?;
        }
        if !flag(row, "pending")?
            || !flag(row, "held_cleanup")?
            || !flag(row, "original_latched")?
            || number(row, "expected_epoch")? != self.txn.expected_epoch
            || number(row, "deadline_ns")? != deadline
            || some_number(row, "logical_committed_at")? != self.logical
            || some_number(row, "owner_epoch")? != self.owner_epoch
            || some_number(row, "sampled_epoch")? != self.sampled_epoch
        {
            return Err("原Cleanup/Pending/事务身份或提交实值改变".into());
        }
        if text(row, "before_command")? != "None" {
            equal_text(row, "before_command_txn_id", &self.txn.txn_id.to_string())?;
            equal_text(row, "before_command_latched", "Some(true)")?;
            if number(row, "before_command_epoch")? != self.txn.expected_epoch {
                return Err("原在飞命令epoch错配".into());
            }
        }
        Ok(())
    }

    fn in_flight(&self, intent: &str) -> String {
        format!(
            "Some(InFlight {{ command: ResyncRemoval {{ txn: {:?} }}, intent: {intent}, expected: Expected {{ target: None, kind: None }} }})",
            self.txn
        )
    }
    fn retry_flight(&self, counted: bool) -> String {
        format!(
            "Some(RetryFlight {{ txn: {:?}, command: ResyncRemoval {{ txn: {:?} }}, stage: Resyncing, expected: Expected {{ target: None, kind: None }}, counted: {counted} }})",
            self.txn, self.txn
        )
    }
}

#[derive(Default)]
struct Progress {
    count: u64,
    stage: String,
    now: u64,
    due: u64,
    next_stage: usize,
    dispatches: Vec<Value>,
    accepted: Vec<Value>,
    awaits: Vec<Value>,
    stuck: Option<Value>,
    shutdown: Option<Value>,
}

pub(super) fn validate_facts(
    logs: &str,
    request_id: &str,
    deadline_ns: u64,
    after_shutdown: bool,
) -> Result<Value, String> {
    let rows = records(logs, IPC)?;
    let identity = Identity::read(rows.first().ok_or("缺原IPC责任日志")?)?;
    let mut progress = Progress {
        stage: "Preparing".into(),
        ..Progress::default()
    };
    for (index, row) in rows.iter().enumerate() {
        identity.validate(row, request_id, deadline_ns)?;
        progress.observe(row, index, &identity, after_shutdown)?;
    }
    if progress.count != 8
        || progress.accepted.len() != 8
        || progress.dispatches.len() != 8
        || progress.awaits.len() != 9
        || progress.next_stage != 1
        || progress.stuck.is_none()
        || progress.shutdown.is_some() != after_shutdown
    {
        return Err("原接受0→8、8次派发、9次due或Stuck/首次shutdown链不完整".into());
    }
    if number(progress.stuck.as_ref().ok_or("缺原Stuck")?, "inputs_now_ns")? <= deadline_ns {
        return Err("原Stuck没有跨过本请求deadline".into());
    }
    let rr = validate_rr(
        &records(logs, RR)?,
        &identity,
        request_id,
        deadline_ns,
        after_shutdown,
    )?;
    let exit = if after_shutdown {
        let exit = validate_exit_facts(logs)?;
        let shutdown = progress.shutdown.as_ref().ok_or("缺原首次shutdown")?;
        if number(&exit, "shutdown_generation")? != number(shutdown, "shutdown_generation")? {
            return Err("原IPC与退出generation不一致".into());
        }
        Some(exit)
    } else {
        None
    };
    Ok(
        json!({"txn_id":identity.txn.txn_id.to_string(),"expected_epoch":identity.txn.expected_epoch,
        "route":identity.route,"logical_committed_at":identity.logical,"accepted":progress.accepted,
        "retry_dispatches":progress.dispatches,"await_retry":progress.awaits,"stuck":progress.stuck,
        "first_shutdown":progress.shutdown,"rr":rr,"exit":exit,"raw_ipc_facts":rows}),
    )
}

impl Progress {
    fn observe(
        &mut self,
        row: &Value,
        index: usize,
        identity: &Identity,
        after_shutdown: bool,
    ) -> Result<(), String> {
        let now = number(row, "inputs_now_ns")?;
        let before = number(row, "before_count")?;
        let after = number(row, "after_count")?;
        let before_stage = text(row, "before_stage")?;
        let after_stage = text(row, "after_stage")?;
        if number(row, "fact_sequence")?
            != u64::try_from(index).map_err(|error| error.to_string())? + 1
            || before != self.count
            || before_stage != self.stage
            || now < self.now
            || number(row, "observed_at_ns")? < now
            || after > 8
            || after < before
            || after > before + 1
        {
            return Err("原fact序列、连续计数、stage或观测时刻不符".into());
        }
        let generation = number(row, "shutdown_generation")?;
        let draining = flag(row, "draining")?;
        match generation {
            0 if !flag(row, "first_shutdown")? && !draining && self.stuck.is_none() => {}
            // generation 在 advance 后读，draining 在其前读，信号可夹在两读之间。
            1.. if after_shutdown
                && flag(row, "first_shutdown")?
                && self.stuck.is_some()
                && self.shutdown.is_none()
                && before_stage == "Stuck"
                && after_stage == "Stuck" =>
            {
                self.shutdown = Some(row.clone());
            }
            _ => return Err("真实Stuck前发生shutdown或首次shutdown重复/缺失".into()),
        }
        if after != before {
            self.observe_acceptance(row, identity, after)?;
        }
        self.observe_transition(row, identity, index)?;
        self.count = after;
        after_stage.clone_into(&mut self.stage);
        self.now = now;
        Ok(())
    }

    fn observe_acceptance(
        &mut self,
        row: &Value,
        identity: &Identity,
        after: u64,
    ) -> Result<(), String> {
        if after != u64::try_from(self.accepted.len()).map_err(|error| error.to_string())? + 1
            || self.dispatches.len() != self.accepted.len() + 1
        {
            return Err("原接受数未逐次0→8或没有对应原派发".into());
        }
        for (key, value) in [
            ("before_command", "Resync"),
            ("before_intent", "Retry"),
            ("before_flight_counted", "Some(false)"),
        ] {
            equal_text(row, key, value)?;
        }
        equal_text(row, "before_in_flight", &identity.in_flight("Retry"))?;
        equal_text(row, "before_flight", &identity.retry_flight(false))?;
        self.accepted
            .push(json!({"writer_source":after+3,"fact":row}));
        Ok(())
    }

    fn observe_transition(
        &mut self,
        row: &Value,
        identity: &Identity,
        index: usize,
    ) -> Result<(), String> {
        let before = text(row, "before_stage")?;
        let after = text(row, "after_stage")?;
        match (before, after) {
            ("Preparing", "FinalizeCleanup") if index == 0 && number(row, "after_count")? == 0 => {
                equal_text(row, "after_command", "RemoveMarker")?;
                equal_text(row, "after_intent", "NextStage")?;
            }
            ("FinalizeCleanup", "Resyncing")
                if self.next_stage == 0 && number(row, "after_count")? == 0 =>
            {
                equal_text(row, "after_command", "Resync")?;
                equal_text(row, "after_intent", "NextStage")?;
                equal_text(row, "in_flight", &identity.in_flight("NextStage"))?;
                equal_text(row, "flight", "None")?;
                self.next_stage += 1;
            }
            ("Resyncing", "AwaitRetry") => self.observe_due(row, identity)?,
            ("AwaitRetry", "Resyncing") => self.observe_dispatch(row, identity)?,
            ("Resyncing", "Resyncing")
                if number(row, "after_count")? == number(row, "before_count")? + 1 =>
            {
                equal_text(row, "in_flight", &identity.in_flight("Retry"))?;
                equal_text(row, "flight", &identity.retry_flight(true))?;
            }
            ("AwaitRetry", "Stuck") if self.stuck.is_none() => {
                if number(row, "after_count")? != 8
                    || number(row, "before_due")? != self.due
                    || number(row, "inputs_now_ns")? < self.due
                    || self.awaits.len() != 9
                {
                    return Err("原末due未到或接受未耗尽就Stuck".into());
                }
                validate_stuck(row)?;
                self.stuck = Some(row.clone());
            }
            ("Stuck", "Stuck") if self.shutdown.is_some() => validate_stuck(row)?,
            _ => return Err(format!("原固定场景出现非法stage转换：{before}→{after}")),
        }
        if after != "AwaitRetry" && number(row, "after_due")? != 0 {
            return Err("非AwaitRetry阶段出现伪due".into());
        }
        Ok(())
    }

    fn observe_due(&mut self, row: &Value, identity: &Identity) -> Result<(), String> {
        let count = number(row, "after_count")?;
        if count != u64::try_from(self.awaits.len()).map_err(|error| error.to_string())? {
            return Err("原AwaitRetry没有按0…8逐次完成".into());
        }
        let intent = match count {
            0 => "NextStage",
            _ => "Retry",
        };
        equal_text(row, "before_command", "Resync")?;
        equal_text(row, "before_intent", intent)?;
        equal_text(row, "before_in_flight", &identity.in_flight(intent))?;
        for key in ["after_command", "after_intent", "in_flight", "flight"] {
            equal_text(row, key, "None")?;
        }
        let backoff = (200_000_000_u64 << count).min(2_000_000_000);
        let due = number(row, "inputs_now_ns")?
            .checked_add(backoff)
            .ok_or("原due计算溢出")?;
        if number(row, "after_due")? != due {
            return Err("原due未按200ms指数退避/2s封顶".into());
        }
        self.due = due;
        self.awaits
            .push(json!({"writer_source":count+3,"fact":row}));
        Ok(())
    }

    fn observe_dispatch(&mut self, row: &Value, identity: &Identity) -> Result<(), String> {
        if number(row, "inputs_now_ns")? < self.due
            || number(row, "before_due")? != self.due
            || self.dispatches.len() != self.accepted.len()
            || self.dispatches.len() >= 8
            || number(row, "before_count")? != number(row, "after_count")?
        {
            return Err("原Retry提前due、重复派发或发生第九次派发".into());
        }
        for (key, value) in [
            ("before_command", "None"),
            ("before_intent", "None"),
            ("after_command", "Resync"),
            ("after_intent", "Retry"),
        ] {
            equal_text(row, key, value)?;
        }
        equal_text(row, "in_flight", &identity.in_flight("Retry"))?;
        equal_text(row, "flight", &identity.retry_flight(false))?;
        self.dispatches
            .push(json!({"writer_source":self.dispatches.len()+4,"fact":row}));
        Ok(())
    }
}

fn validate_stuck(row: &Value) -> Result<(), String> {
    for (key, value) in [
        ("stage_detail", "Stuck(RepairExhausted)"),
        ("in_flight", "None"),
        ("flight", "None"),
        ("next", "None"),
        ("after_command", "None"),
        ("after_intent", "None"),
    ] {
        equal_text(row, key, value)?;
    }
    if number(row, "after_count")? != 8 {
        return Err("原Stuck接受数不为8".into());
    }
    Ok(())
}

fn validate_rr(
    rows: &[Value],
    identity: &Identity,
    request_id: &str,
    deadline: u64,
    stopped: bool,
) -> Result<Value, String> {
    if rows.len() != 1 + usize::from(stopped) {
        return Err("原RR queued/stopped记录缺失或重复".into());
    }
    let queued = rows.first().ok_or("缺原RR queued")?;
    let ledger = text(queued, "ledger")?;
    let peer = text(queued, "slot_peer")?;
    for (index, row) in rows.iter().enumerate() {
        for (key, value) in [
            (
                "boundary",
                match index {
                    0 => "queued",
                    _ => "stopped",
                },
            ),
            ("request_id", request_id),
            ("route", &identity.route),
            ("class", "Restore"),
            ("ledger", ledger),
            ("slot_peer", peer),
            ("active_peer", peer),
        ] {
            equal_text(row, key, value)?;
        }
        for key in ["pending_count", "total", "restore"] {
            if number(row, key)? != 1 {
                return Err(format!("原RR{key}丢失或伪退款"));
            }
        }
        for key in ["residual_count", "lower", "normal"] {
            if number(row, key)? != 0 {
                return Err(format!("原RR{key}出现额外责任"));
            }
        }
        for (key, value) in [
            ("peer_total", 1),
            ("peer_restore", 1),
            ("peer_lower", 0),
            ("peer_normal", 0),
            ("deadline_ns", deadline),
        ] {
            if some_number(row, key)? != value {
                return Err(format!("原RR{key}与原责任不符"));
            }
        }
        if !flag(row, "peer_matches")?
            || !flag(row, "active_decoded")?
            || number(row, "observed_at_ns")? == 0
        {
            return Err("原Active/Slot peer或请求解码不符".into());
        }
        if index == 1
            && (number(row, "observed_at_ns")? <= deadline
                || number(row, "observed_at_ns")? < number(queued, "observed_at_ns")?)
        {
            return Err("原停止准入不在同一请求deadline之后".into());
        }
    }
    Ok(json!(rows))
}

pub(super) fn validate_exit_facts(logs: &str) -> Result<Value, String> {
    let rows = records(logs, EXIT)?;
    if rows.len() != 1 {
        return Err("原退出事实缺失或重复".into());
    }
    let row = &rows[0];
    for (key, value) in [
        ("writer_join_result", "missing"),
        ("authority_phase", "Some(ClearFinalizing)"),
        ("flush_outcome", "None"),
    ] {
        equal_text(row, key, value)?;
    }
    if number(row, "shutdown_generation")? == 0
        || number(row, "lowering_ack_epoch")? != 0
        || flag(row, "clear_retired")?
        || flag(row, "writer_fallback")?
    {
        return Err("原generation/ACK/Clear退休或writer兜底事实不符".into());
    }
    let raw = text(row, "shutdown_failures")?;
    let mut actual: Vec<_> = raw
        .strip_prefix('[')
        .and_then(|raw| raw.strip_suffix(']'))
        .ok_or("原失败集合不是完整Debug列表")?
        .split(", ")
        .map(str::to_owned)
        .collect();
    let mut expected = vec![format!("{:?}", ShutdownFailure::LoweringUnconfirmed)];
    expected.extend(
        [
            HandleKind::ClearRr,
            HandleKind::Executor,
            HandleKind::Ipc,
            HandleKind::Watchdog,
            HandleKind::LatchWriter,
        ]
        .map(|kind| format!("{:?}", ShutdownFailure::JoinTimeout(kind))),
    );
    actual.sort();
    expected.sort();
    if actual != expected {
        return Err(format!("原失败集合缺项、额外panic或错误JoinTimeout：{raw}"));
    }
    Ok(row.clone())
}

#[cfg(test)]
mod tests {
    use super::super::super::w4_latch_crash::StepFact;
    use super::*;

    const REQUEST: &str = "原请求";
    const DEADLINE: u64 = 6_000_000_000;
    const TXN: ClearTxnIdentity = ClearTxnIdentity {
        txn_id: 1_u128 << 100,
        expected_epoch: 7,
        original_latched: true,
    };

    fn identity() -> Identity {
        Identity {
            txn: TXN,
            route: "Some(原route)".into(),
            logical: 900_000_000,
            owner_domain: "Some(原domain)".into(),
            owner_epoch: 8,
            sampled_epoch: 8,
        }
    }

    // 合成日志只测试判据边界，不作为原 daemon 或接受动作的运行证据。
    fn row(before: &str, after: &str, before_count: u64, after_count: u64, now: u64) -> Value {
        let id = identity();
        let mut row = json!({"message":IPC,"fact_sequence":0,"inputs_now_ns":now,"observed_at_ns":now,
            "draining":false,"first_shutdown":false,"shutdown_generation":0,
            "txn_id":TXN.txn_id.to_string(),"expected_epoch":TXN.expected_epoch,"original_latched":true,
            "before_txn":format!("Some({TXN:?})"),"before_stage":before,"after_stage":after,
            "before_due":0,"after_due":0,"before_count":before_count,"after_count":after_count,
            "before_command":"None","before_intent":"None","before_command_txn_id":"0",
            "before_command_epoch":0,"before_command_latched":"None","before_in_flight":"None",
            "before_flight":"None","before_flight_counted":"None","after_command":"None","after_intent":"None"});
        let owner = json!({
            "in_flight":"None","flight":"None","next":"None","stage_detail":after,
            "held_cleanup":true,"cleanup_kind":"Some(Finalize)","owner_kind":"Finalizing",
            "owner_domain":id.owner_domain,"owner_epoch":"Some(8)","sampled_epoch":"Some(8)",
            "owner_original_latched":"Some(true)","pending":true,"request_id":REQUEST,"route":id.route,
            "deadline_ns":DEADLINE,"logical_committed_at":"Some(900000000)","physical_done_at":"None",
            "locked":"Some(Cleared { cleared_at_ns: 900000000, superseded: false })"});
        row.as_object_mut()
            .unwrap()
            .extend(owner.as_object().unwrap().clone());
        row
    }

    fn before_resync(row: &mut Value, intent: &str, counted: Option<bool>) {
        row["before_command"] = json!("Resync");
        row["before_intent"] = json!(intent);
        row["before_command_txn_id"] = json!(TXN.txn_id.to_string());
        row["before_command_epoch"] = json!(TXN.expected_epoch);
        row["before_command_latched"] = json!("Some(true)");
        row["before_in_flight"] = json!(identity().in_flight(intent));
        if let Some(counted) = counted {
            row["before_flight"] = json!(identity().retry_flight(counted));
            row["before_flight_counted"] = json!(format!("Some({counted})"));
        }
    }
    fn after_resync(row: &mut Value, intent: &str, counted: Option<bool>) {
        row["after_command"] = json!("Resync");
        row["after_intent"] = json!(intent);
        row["in_flight"] = json!(identity().in_flight(intent));
        if let Some(counted) = counted {
            row["flight"] = json!(identity().retry_flight(counted));
        }
    }

    fn ipc_rows(post: bool) -> Vec<Value> {
        let mut first = row("Preparing", "FinalizeCleanup", 0, 0, 1_000_000_000);
        first["after_command"] = json!("RemoveMarker");
        first["after_intent"] = json!("NextStage");
        let mut next = row("FinalizeCleanup", "Resyncing", 0, 0, 1_010_000_000);
        after_resync(&mut next, "NextStage", None);
        let mut rows = vec![first, next];
        let mut now = 1_020_000_000;
        for count in 0..=8 {
            let mut await_row = row("Resyncing", "AwaitRetry", count, count, now);
            let intent = match count {
                0 => "NextStage",
                _ => "Retry",
            };
            before_resync(&mut await_row, intent, (count != 0).then_some(true));
            let due = now + (200_000_000_u64 << count).min(2_000_000_000);
            await_row["after_due"] = json!(due);
            rows.push(await_row);
            if count == 8 {
                let mut stuck = row("AwaitRetry", "Stuck", 8, 8, due);
                stuck["before_due"] = json!(due);
                stuck["stage_detail"] = json!("Stuck(RepairExhausted)");
                rows.push(stuck);
                if post {
                    let mut shutdown = row("Stuck", "Stuck", 8, 8, due + 1);
                    shutdown["stage_detail"] = json!("Stuck(RepairExhausted)");
                    shutdown["first_shutdown"] = json!(true);
                    shutdown["shutdown_generation"] = json!(9);
                    rows.push(shutdown);
                }
                break;
            }
            let mut dispatch = row("AwaitRetry", "Resyncing", count, count, due);
            dispatch["before_due"] = json!(due);
            after_resync(&mut dispatch, "Retry", Some(false));
            rows.push(dispatch);
            let mut claim = row("Resyncing", "Resyncing", count, count + 1, due + 1);
            before_resync(&mut claim, "Retry", Some(false));
            after_resync(&mut claim, "Retry", Some(true));
            rows.push(claim);
            now = due + 2;
        }
        for (index, row) in rows.iter_mut().enumerate() {
            row["fact_sequence"] = json!(index + 1);
        }
        rows
    }

    fn rr_row(stopped: bool) -> Value {
        json!({"message":RR,"boundary":if stopped {"stopped"} else {"queued"},
            "observed_at_ns":if stopped {20_000_000_000_u64} else {990_000_000},
            "request_id":REQUEST,"route":identity().route,"class":"Restore","ledger":"原ledger",
            "slot_peer":"原peer","active_peer":"原peer","peer_matches":true,"active_decoded":true,
            "pending_count":1,"total":1,"restore":1,"lower":0,"normal":0,"residual_count":0,
            "peer_total":"Some(1)","peer_restore":"Some(1)","peer_lower":"Some(0)","peer_normal":"Some(0)",
            "deadline_ns":format!("Some({DEADLINE})")})
    }
    fn exit_row() -> Value {
        json!({"message":EXIT,"writer_join_result":"missing","shutdown_generation":9,
            "lowering_ack_epoch":0,"authority_phase":"Some(ClearFinalizing)","clear_retired":false,
            "flush_outcome":"None","writer_fallback":false,
            "shutdown_failures":"[LoweringUnconfirmed, JoinTimeout(ClearRr), JoinTimeout(Executor), JoinTimeout(Ipc), JoinTimeout(Watchdog), JoinTimeout(LatchWriter)]"})
    }
    fn logs(rows: &[Value], post: bool) -> String {
        let mut all = rows.to_vec();
        all.push(rr_row(false));
        if post {
            all.push(rr_row(true));
            all.push(exit_row());
        }
        all.iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn separate_claim_count_and_shutdown_between_generation_reads_are_accepted() {
        assert!(validate_facts(&logs(&ipc_rows(false), false), REQUEST, DEADLINE, false).is_ok());
        let rows = ipc_rows(true);
        assert_eq!(rows.last().unwrap()["draining"], false);
        assert!(validate_facts(&logs(&rows, true), REQUEST, DEADLINE, true).is_ok());
    }

    #[test]
    fn claim_and_completion_in_one_advance_are_also_accepted() {
        let rows = ipc_rows(false);
        let mut combined = Vec::new();
        let mut claim = None;
        for mut row in rows {
            if row["before_stage"] == "Resyncing" && row["after_stage"] == "Resyncing" {
                claim = Some(row);
                continue;
            }
            if let Some(previous) = claim.take() {
                row["before_count"] = previous["before_count"].clone();
                before_resync(&mut row, "Retry", Some(false));
            }
            row["fact_sequence"] = json!(combined.len() + 1);
            combined.push(row);
        }
        assert!(validate_facts(&logs(&combined, false), REQUEST, DEADLINE, false).is_ok());
    }

    #[test]
    fn wrong_count_early_due_and_mismatched_retry_identity_are_rejected() {
        for mutate in [
            ("after_count", json!(2)),
            ("before_flight_counted", json!("Some(true)")),
            ("before_command_txn_id", json!("17")),
            ("before_flight", json!("None")),
        ] {
            let mut rows = ipc_rows(false);
            let row = rows
                .iter_mut()
                .find(|row| row["after_count"] == 1 && row["before_count"] == 0)
                .unwrap();
            row[mutate.0] = mutate.1;
            assert!(validate_facts(&logs(&rows, false), REQUEST, DEADLINE, false).is_err());
        }
        let mut rows = ipc_rows(false);
        let row = rows
            .iter_mut()
            .find(|row| row["before_stage"] == "AwaitRetry" && row["after_stage"] == "Resyncing")
            .unwrap();
        row["inputs_now_ns"] = json!(row["before_due"].as_u64().unwrap() - 1);
        assert!(validate_facts(&logs(&rows, false), REQUEST, DEADLINE, false).is_err());
    }

    #[test]
    fn fake_refund_retirement_owner_change_and_duplicate_shutdown_are_rejected() {
        let rows = ipc_rows(true);
        let mut rr = rr_row(true);
        rr["restore"] = json!(0);
        assert!(validate_rr(&[rr_row(false), rr], &identity(), REQUEST, DEADLINE, true).is_err());
        for (key, value) in [
            ("physical_done_at", "Some(123)"),
            ("owner_domain", "Some(另一个domain)"),
            ("locked", "None"),
            ("route", "Some(另一个route)"),
        ] {
            let mut bad = rows.clone();
            bad[4][key] = json!(value);
            assert!(validate_facts(&logs(&bad, true), REQUEST, DEADLINE, true).is_err());
        }
        let mut bad = rows.clone();
        let mut duplicate = bad.last().unwrap().clone();
        duplicate["fact_sequence"] = json!(bad.len() + 1);
        bad.push(duplicate);
        assert!(validate_facts(&logs(&bad, true), REQUEST, DEADLINE, true).is_err());
    }

    #[test]
    fn exit_wrong_phase_ack_fallback_and_failure_set_are_rejected() {
        for (key, value) in [
            ("authority_phase", json!("Idle")),
            ("lowering_ack_epoch", json!(9)),
            ("writer_fallback", json!(true)),
            ("writer_join_result", json!("stopped")),
            (
                "shutdown_failures",
                json!("[LoweringUnconfirmed, JoinTimeout(Io)]"),
            ),
        ] {
            let mut row = exit_row();
            row[key] = value;
            assert!(validate_exit_facts(&row.to_string()).is_err());
        }
        assert!(validate_exit_facts(&exit_row().to_string()).is_ok());
    }

    fn chain() -> Vec<CommandFact> {
        (0..11)
            .map(|index| {
                let (command, before, after, completion, stages, slot): (_, _, _, _, &[S], _) =
                    match index {
                        0 => (
                            C::Prepare,
                            T::None,
                            T::Prepared,
                            K::Prepared,
                            &PREPARE,
                            None,
                        ),
                        1 => (
                            C::RemoveCommitted,
                            T::Prepared,
                            T::Unsynced,
                            K::RemoveFailedUnsynced,
                            &REMOVE,
                            Some(0),
                        ),
                        _ => (
                            C::Resync,
                            T::Unsynced,
                            T::Unsynced,
                            K::RemoveFailedUnsynced,
                            &[S::SyncRemovalDirectory],
                            Some(1),
                        ),
                    };
                CommandFact {
                    source: index + 1,
                    txn: TXN,
                    command,
                    before,
                    after,
                    completion,
                    failure: slot.map(|_| (S::SyncRemovalDirectory, StepPhase::Operation)),
                    steps: stages
                        .iter()
                        .map(|stage| {
                            let slot = if *stage == S::SyncRemovalDirectory {
                                slot
                            } else {
                                None
                            };
                            StepFact {
                                stage: *stage,
                                succeeded: slot.is_none(),
                                slot,
                            }
                        })
                        .collect(),
                }
            })
            .collect()
    }
    #[test]
    fn incomplete_prepare_extra_scope_wrong_slot_and_fake_retirement_are_rejected() {
        assert!(validate_chain(&chain()).is_ok());
        let mut bad = chain();
        bad[0].steps.remove(5);
        assert!(validate_chain(&bad).is_err());
        let mut bad = chain();
        bad[1].steps.remove(0);
        assert!(validate_chain(&bad).is_err());
        let mut bad = chain();
        bad[10].after = T::None;
        assert!(validate_chain(&bad).is_err());
        let mut bad = chain();
        bad[5].steps[0].slot = Some(0);
        assert!(validate_chain(&bad).is_err());
        let mut bad = chain();
        bad.extend(chain().into_iter().take(1));
        assert!(validate_chain(&bad).is_err());
    }
}
