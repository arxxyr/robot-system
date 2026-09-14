//! W4a 两条真实目录同步故障链；不把进程样例称为断电或自然存储故障。

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::time::Duration;

use iceoryx2::prelude::NodeBuilder;
use iceoryx2::service::ipc::Service;
use iceoryx2::strict::StrictChannelStatus;
use robot_core::time::{RobotClock, SystemClock};
use robot_msg::{markers, msg};
use robot_s0::clear_estop_transaction::{ClearTxnStage, StepPhase};
use robot_s0::comm_fault_trace::w4::{CommandKind, CompletionKind, TokenKind, W4Payload};
use robot_s0::comm_fault_trace::{SampleMode, TraceEvent, TracePage, w4};
use robot_s0::latch_writer::ClearTxnIdentity;
use robot_transport::iceoryx2::request_response::{Iceoryx2PendingResponse, Iceoryx2RrClient};
use serde_json::{Value, json};
use tokio::process::Child;

use super::Args;
use super::control::ControlPort;
use super::harness::{Case, stop, wait_log};
use super::report::Report;
use super::sla::send_sample_request;

const CASE_FILTER: &str = "W4-FILE-FAULT-SMOKE";
const MAX_W4_EVENTS: usize = 1024;
const MAX_COMMANDS: usize = 16;
const REPLY_WAIT_NS: u64 = 3_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Created,
    CreatedUnsynced,
}

impl Mode {
    const fn label(self) -> &'static str {
        match self {
            Self::Created => "created_operation_failure_retires_original_fsync_failure",
            Self::CreatedUnsynced => {
                "created_unsynced_repair_resumes_same_target_and_retires_first_failure"
            }
        }
    }

    const fn configuration(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::CreatedUnsynced => "created-unsynced",
        }
    }
}

pub(super) async fn run(args: &Args, report: &mut Report) -> Result<(), String> {
    if !cfg!(target_os = "linux") {
        return Err("W4a O_PATH 真实目录同步样例仅支持 Linux".into());
    }
    if args.case.as_deref() != Some(CASE_FILTER) {
        return Err("W4a 只接受明确的 W4-FILE-FAULT-SMOKE 筛选，拒绝零例通过".into());
    }
    let before = report.cases.len();
    for mode in [Mode::Created, Mode::CreatedUnsynced] {
        let started = SystemClock.now().0;
        let result = match mode {
            Mode::Created => created_operation_failure_retires_original_fsync_failure(args).await,
            Mode::CreatedUnsynced => {
                created_unsynced_repair_resumes_same_target_and_retires_first_failure(args).await
            }
        };
        report.record(mode.label(), CASE_FILTER, started, result);
    }
    if report.cases.len().checked_sub(before) != Some(2) {
        return Err("W4a 实际登记数量不是两条完整样例".into());
    }
    Ok(())
}

async fn created_operation_failure_retires_original_fsync_failure(
    args: &Args,
) -> Result<Value, String> {
    run_case(args, Mode::Created).await
}

async fn created_unsynced_repair_resumes_same_target_and_retires_first_failure(
    args: &Args,
) -> Result<Value, String> {
    run_case(args, Mode::CreatedUnsynced).await
}

#[derive(Debug)]
struct StepFact {
    stage: ClearTxnStage,
    succeeded: bool,
    slot: Option<u8>,
}

#[derive(Debug)]
struct CommandFact {
    source: u64,
    txn: ClearTxnIdentity,
    command: CommandKind,
    before: TokenKind,
    after: TokenKind,
    completion: CompletionKind,
    failure: Option<(ClearTxnStage, StepPhase)>,
    steps: Vec<StepFact>,
}

struct PayloadCursor<'a> {
    payloads: &'a [W4Payload],
    index: usize,
}

impl PayloadCursor<'_> {
    fn next(&mut self) -> Result<W4Payload, String> {
        let value = self
            .payloads
            .get(self.index)
            .copied()
            .ok_or_else(|| "W4 命令或步骤事件未完整配对".to_owned())?;
        self.index += 1;
        Ok(value)
    }

    fn expect(&mut self, expected: W4Payload) -> Result<(), String> {
        let actual = self.next()?;
        if actual != expected {
            return Err(format!(
                "W4 事件顺序/事实不符：期望{expected:?}，实际{actual:?}"
            ));
        }
        Ok(())
    }
}

/// 对原顺序逐条配对；任何未知事件、重复、缺失、错代次或越界都不能跳过。
fn analyze_trace(events: &[TraceEvent], mode: Mode) -> Result<Value, String> {
    if events.is_empty() || events.len() > MAX_W4_EVENTS {
        return Err("W4 事件为空或超过原1024条上限".into());
    }
    let mut commands = Vec::with_capacity(MAX_COMMANDS);
    let mut index = 0;
    let mut last_stamp = 0;
    let sample = events[0].sample_seq;
    while index < events.len() {
        if commands.len() == MAX_COMMANDS {
            return Err("W4 命令超过原16个scope上限".into());
        }
        let source = u64::try_from(commands.len()).map_err(|e| e.to_string())? + 1;
        let mut payloads = Vec::new();
        loop {
            let event = events.get(index).ok_or("原命令缺CommandEnd")?;
            if event.source != source
                || sample == 0
                || event.sample_seq != sample
                || event.stamp_ns < last_stamp
            {
                return Err(format!("W4 原scope/sample/时钟顺序不符：{event:?}"));
            }
            last_stamp = event.stamp_ns;
            let payload = w4::decode(event)
                .map_err(str::to_owned)?
                .ok_or("W4 解析输入混入非W4事件")?;
            if let W4Payload::Failure { .. } = payload {
                return Err(format!("实际夹具失败不能算故障命中：{payload:?}"));
            }
            payloads.push(payload);
            index += 1;
            if matches!(payload, W4Payload::CommandEnd { .. }) {
                break;
            }
        }
        let fact = parse_command(source, &payloads)?;
        if fact.txn.txn_id == 0 || !fact.txn.original_latched {
            return Err(format!("原事务不是本例真实latched事务：{:?}", fact.txn));
        }
        if commands
            .first()
            .is_some_and(|first: &CommandFact| first.txn != fact.txn)
        {
            return Err("不同scope未保留完整原128位txn/epoch/latch身份".into());
        }
        commands.push(fact);
    }
    validate_chain(&commands, mode)?;
    Ok(json!({"txn_id": commands[0].txn.txn_id.to_string(),
        "expected_epoch": commands[0].txn.expected_epoch, "original_latched": true,
        "command_scopes": commands.iter().map(|command| json!({
            "source": command.source, "command": format!("{:?}", command.command),
            "token_before": format!("{:?}", command.before),
            "token_after": format!("{:?}", command.after),
            "completion": format!("{:?}", command.completion),
            "failure": format!("{:?}", command.failure),
            "steps": command.steps.iter().map(|step| json!({
                "stage": format!("{:?}", step.stage), "succeeded": step.succeeded,
                "fault_slot": step.slot})).collect::<Vec<_>>()
        })).collect::<Vec<_>>(), "remove_marker_successes": 1}))
}

fn parse_command(source: u64, payloads: &[W4Payload]) -> Result<CommandFact, String> {
    let mut cursor = PayloadCursor { payloads, index: 0 };
    let W4Payload::CommandBegin { txn, command } = cursor.next()? else {
        return Err("scope首事件不是CommandBegin".into());
    };
    let W4Payload::Token {
        after: false,
        token: before,
    } = cursor.next()?
    else {
        return Err("scope未完整观察原TokenBefore".into());
    };
    let mut steps = Vec::new();
    let mut occurrences = [0_u64; 2];
    while let Some(W4Payload::Stage {
        succeeded: false, ..
    }) = payloads.get(cursor.index)
    {
        let next_seq = u64::try_from(steps.len()).map_err(|e| e.to_string())? + 1;
        steps.push(parse_step(&mut cursor, next_seq, &mut occurrences)?);
    }
    let W4Payload::Completion {
        kind: completion,
        failure,
    } = cursor.next()?
    else {
        return Err("原步骤末尾缺同次Completion".into());
    };
    let W4Payload::Token {
        after: true,
        token: after,
    } = cursor.next()?
    else {
        return Err("原Completion之后缺同次TokenAfter".into());
    };
    cursor.expect(W4Payload::CommandEnd { completed: true })?;
    if cursor.index != payloads.len() {
        return Err("CommandEnd后还有未分类事件".into());
    }
    let failed = steps
        .iter()
        .filter(|step| !step.succeeded)
        .collect::<Vec<_>>();
    match (failed.as_slice(), failure) {
        ([], None) => {}
        ([step], Some((stage, StepPhase::Operation)))
            if step.stage == stage && steps.last().is_some_and(|last| !last.succeeded) => {}
        other => return Err(format!("原步骤结果和completion失败来源不符：{other:?}")),
    }
    Ok(CommandFact {
        source,
        txn,
        command,
        before,
        after,
        completion,
        failure,
        steps,
    })
}

fn parse_step(
    cursor: &mut PayloadCursor<'_>,
    next_seq: u64,
    occurrences: &mut [u64; 2],
) -> Result<StepFact, String> {
    let W4Payload::Stage {
        succeeded: false,
        step_seq,
        stage,
        occurrence,
    } = cursor.next()?
    else {
        return Err("文件步骤缺原BeforeEntered".into());
    };
    let expected_occurrence = match stage {
        ClearTxnStage::SyncMarkerDirectory => {
            occurrences[0] += 1;
            occurrences[0]
        }
        ClearTxnStage::SyncRemovalDirectory => {
            occurrences[1] += 1;
            occurrences[1]
        }
        _ => 0,
    };
    if step_seq != next_seq || occurrence != expected_occurrence {
        return Err("原step_seq/目标occurrence重复、跳跃或错配".into());
    }
    #[allow(
        clippy::single_match_else,
        reason = "按事件枚举分派故障链与成功链，集中保持原游标消费顺序便于审计"
    )]
    let slot = match cursor.payloads.get(cursor.index).copied() {
        Some(W4Payload::FaultSelected { slot, .. }) => {
            cursor.expect(W4Payload::FaultSelected {
                slot,
                step_seq,
                stage,
                occurrence,
            })?;
            let W4Payload::Alias {
                verified: false,
                slot: expected_slot,
                step_seq: expected_step,
                dev,
                ino,
            } = cursor.next()?
            else {
                return Err("别名准备缺Expected身份".into());
            };
            if (expected_slot, expected_step) != (slot, step_seq) {
                return Err("别名Expected不属于原选中步骤".into());
            }
            cursor.expect(W4Payload::Alias {
                verified: true,
                slot,
                step_seq,
                dev,
                ino,
            })?;
            cursor.expect(W4Payload::DirSyncCall { slot, step_seq })?;
            cursor.expect(W4Payload::DirSyncReturned {
                slot,
                step_seq,
                success: false,
                errno: Some(libc::EBADF),
            })?;
            Some(slot)
        }
        _ => {
            cursor.expect(W4Payload::Stage {
                succeeded: true,
                step_seq,
                stage,
                occurrence,
            })?;
            None
        }
    };
    Ok(StepFact {
        stage,
        succeeded: slot.is_none(),
        slot,
    })
}

#[allow(
    clippy::too_many_lines,
    reason = "同一原事务的命令、凭证与完整文件步骤需集中核对，避免拆散前后顺序断言"
)]
fn validate_chain(commands: &[CommandFact], mode: Mode) -> Result<(), String> {
    use ClearTxnStage as S;
    use CommandKind as C;
    use CompletionKind as R;
    use TokenKind as T;
    let expected = match mode {
        Mode::Created => vec![
            (C::Prepare, T::None, T::Created, R::PrepareCreated),
            (
                C::RepairLatched,
                T::Created,
                T::NoMarker,
                R::RepairedLatched,
            ),
            (C::RemoveRepaired, T::NoMarker, T::None, R::RemovedRepaired),
        ],
        Mode::CreatedUnsynced => vec![
            (C::Prepare, T::None, T::Created, R::PrepareCreated),
            (
                C::RepairLatched,
                T::Created,
                T::UnsyncedBeforeRepairLatched,
                R::RepairFailedKept,
            ),
            (
                C::RepairLatched,
                T::UnsyncedBeforeRepairLatched,
                T::NoMarker,
                R::RepairedLatched,
            ),
            (C::RemoveRepaired, T::NoMarker, T::None, R::RemovedRepaired),
        ],
    };
    if commands.len() != expected.len() {
        return Err("原命令链尚未完整或存在额外重投".into());
    }
    for (actual, expected) in commands.iter().zip(expected) {
        if (
            actual.command,
            actual.before,
            actual.after,
            actual.completion,
        ) != expected
        {
            return Err(format!("原token/target/completion链不符：{actual:?}"));
        }
    }
    let prepare = [
        S::CreateMarker,
        S::SampleMarkerIdentity,
        S::WriteMarker,
        S::SyncMarkerFile,
        S::SyncMarkerDirectory,
    ];
    if !commands[0].steps.iter().map(|step| step.stage).eq(prepare) {
        return Err("原Prepare五步观测缺失或多出步骤".into());
    }
    let created_prefix = [
        S::ReadMarker,
        S::ValidateMarker,
        S::RemoveMarker,
        S::SyncRemovalDirectory,
    ];
    let first_repair = commands[1]
        .steps
        .iter()
        .map(|step| step.stage)
        .collect::<Vec<_>>();
    if !first_repair.starts_with(&created_prefix) {
        return Err("原Created身份核对/删除/同步步骤不完整".into());
    }
    let faults = commands
        .iter()
        .flat_map(|command| {
            command
                .steps
                .iter()
                .filter_map(move |step| step.slot.map(|slot| (command.source, step.stage, slot)))
        })
        .collect::<Vec<_>>();
    let expected_faults = match mode {
        Mode::Created => vec![(1, S::SyncMarkerDirectory, 0)],
        Mode::CreatedUnsynced => vec![
            (1, S::SyncMarkerDirectory, 0),
            (2, S::SyncRemovalDirectory, 1),
        ],
    };
    if faults != expected_faults {
        return Err(format!("两固定slot真实命中不符：{faults:?}"));
    }
    let removals = commands
        .iter()
        .flat_map(|command| &command.steps)
        .filter(|step| step.stage == S::RemoveMarker && step.succeeded)
        .count();
    if removals != 1 {
        return Err(format!("真实unlink成功不是恰好一次：{removals}"));
    }
    let repaired_index = match mode {
        Mode::Created => 1,
        Mode::CreatedUnsynced => 2,
    };
    let repaired = &commands[repaired_index];
    let stages = repaired
        .steps
        .iter()
        .map(|step| step.stage)
        .collect::<Vec<_>>();
    let pair = [
        S::CreatePrimary,
        S::WritePrimary,
        S::SyncPrimaryFile,
        S::RenamePrimary,
        S::SyncPrimaryDirectory,
        S::CreateBackup,
        S::WriteBackup,
        S::SyncBackupFile,
        S::RenameBackup,
        S::SyncBackupDirectory,
    ];
    if !stages.ends_with(&pair) || !repaired.steps.iter().all(|step| step.succeeded) {
        return Err("Repaired前未真实完成原Latched primary→backup全部操作".into());
    }
    match mode {
        Mode::Created => {
            let remove = stages
                .iter()
                .position(|stage| *stage == S::RemoveMarker)
                .ok_or("Created修复缺真实unlink")?;
            if stages.get(remove + 1) != Some(&S::SyncRemovalDirectory)
                || stages.get(remove + 2) != Some(&S::CreatePrimary)
                || stages.len() != created_prefix.len() + pair.len()
            {
                return Err("Created删除/同步后未按原目标立即继续修复".into());
            }
        }
        Mode::CreatedUnsynced => {
            let first = &commands[1].steps;
            if first.len() != created_prefix.len()
                || first[first.len() - 2].stage != S::RemoveMarker
                || first.last().map(|step| step.stage) != Some(S::SyncRemovalDirectory)
                || stages.first() != Some(&S::SyncRemovalDirectory)
                || stages.get(1) != Some(&S::CreatePrimary)
                || stages.contains(&S::RemoveMarker)
                || stages.len() != pair.len() + 1
            {
                return Err("原Unsynced必须先重同步、再原目标修复，不能再次unlink".into());
            }
        }
    }
    if !commands.last().ok_or("缺末命令")?.steps.is_empty() {
        return Err("原NoMarker清理不应再次执行文件删除/同步".into());
    }
    Ok(())
}

struct TraceCapture {
    page: TracePage,
    csv: BufWriter<File>,
    path: PathBuf,
    sample: Option<u64>,
    events: Vec<TraceEvent>,
    total_events: u64,
    first_error: Option<String>,
}

impl TraceCapture {
    fn open(case: &Case) -> Result<Self, String> {
        let path = case.directory.join("w4_trace.csv");
        let mut csv = BufWriter::new(File::create(&path).map_err(|e| e.to_string())?);
        writeln!(
            csv,
            "sample_seq,stamp_ns,kind,source,normal_seq,generation,a,b"
        )
        .map_err(|e| e.to_string())?;
        csv.flush().map_err(|e| e.to_string())?;
        Ok(Self {
            page: case.trace("s0")?,
            csv,
            path,
            sample: None,
            events: Vec::with_capacity(MAX_W4_EVENTS),
            total_events: 0,
            first_error: None,
        })
    }

    fn fail(&mut self, error: String) {
        if self.first_error.is_none() {
            self.first_error = Some(error);
        }
    }

    // 每批和内存解析都受原固定容量约束；超限后继续落原CSV，保留首个采集错误。
    fn drain(&mut self) {
        for event in self.page.drain(robot_s0::comm_fault_trace::TRACE_CAPACITY) {
            if let Err(error) = writeln!(
                self.csv,
                "{},{},{:?},{},{},{},{},{}",
                event.sample_seq,
                event.stamp_ns,
                event.kind,
                event.source,
                event.normal_seq,
                event.generation,
                event.a,
                event.b
            ) {
                self.fail(format!("写入原事件CSV失败：{error}"));
            }
            match self.total_events.checked_add(1) {
                Some(count) => self.total_events = count,
                None => self.fail("事件总数溢出".into()),
            }
            if self.sample != Some(event.sample_seq) || event.sample_seq == 0 {
                self.fail(format!("事件不属于原已确认sample：{event:?}"));
            }
            match w4::decode(&event) {
                Ok(None) => {}
                Ok(Some(_)) => {
                    if self.events.len() < MAX_W4_EVENTS {
                        self.events.push(event);
                    } else {
                        self.fail("W4事件超过原1024条解析上限".into());
                    }
                }
                Err(error) => self.fail(format!("W4事件编码无效：{error}，{event:?}")),
            }
        }
        if self.page.dropped() != 0 {
            self.fail(format!("原事件环发生丢弃：{}", self.page.dropped()));
        }
        if let Err(error) = self.csv.flush() {
            self.fail(format!("保存原事件CSV失败：{error}"));
        }
    }

    fn validate(&self, mode: Mode) -> Result<Value, String> {
        if let Some(error) = &self.first_error {
            return Err(error.clone());
        }
        analyze_trace(&self.events, mode)
    }

    fn evidence(&self) -> Value {
        json!({"csv": self.path, "sample_seq": self.sample,
            "total_events": self.total_events, "w4_events": self.events.len(),
            "dropped": self.page.dropped(), "first_error": self.first_error})
    }
}

async fn run_case(args: &Args, mode: Mode) -> Result<Value, String> {
    let case = Case::new(&args.evidence_dir, &args.bin_dir, mode.label(), 1)?;
    for path in [&case.latch, &case.directory.join("estop_latch.bak")] {
        std::fs::write(path, b"1\n").map_err(|e| e.to_string())?;
    }
    let port = ControlPort::new(&case.robot_id)?;
    let mut command = case.command();
    for name in [
        "NOTIFY_SOCKET",
        "WATCHDOG_USEC",
        "WATCHDOG_PID",
        "INVOCATION_ID",
    ] {
        command.env_remove(name);
    }
    command
        .env("ROBOT_TEST_W4_FILE_FAULT", mode.configuration())
        .env("ROBOT_LOG_FORMAT", "json");
    let mut child = case.spawn("s0", command)?;
    let mut capture = TraceCapture::open(&case);
    let result = match &mut capture {
        Ok(capture) => exercise(&case, &mut child, &port, capture, mode).await,
        Err(error) => Err(error.clone()),
    };
    // 首错误与原Child独立保存；即使采集/解析失败，也不跳过原进程的真实回收。
    let cleanup = close_child(&mut child, capture.as_mut().ok(), || Ok(())).await;
    if let Ok(capture) = &mut capture {
        capture.drain();
    }
    let capture_evidence = match &capture {
        Ok(capture) => capture.evidence(),
        Err(error) => json!({"open_error": error}),
    };
    let final_capture_error = capture.as_ref().ok().and_then(|c| c.first_error.clone());
    let final_state = (|| {
        let page = case.observation("s0")?;
        if page.fired_checkpoint != 0 || page.residual_bitmap != 0 {
            return Err(format!("本例应真实清洁退出，观测不符：{page:?}"));
        }
        verify_files(&case)?;
        Ok(
            json!({"residual_bitmap": page.residual_bitmap, "fired_checkpoint": page.fired_checkpoint}),
        )
    })();
    let summary = json!({"mode": mode.configuration(), "result": result,
        "cleanup": cleanup, "capture": capture_evidence,
        "final_state": final_state,
        "directory": case.directory, "fault_input": "同目录O_PATH别名真实fsync返回EBADF"});
    let saved = serde_json::to_vec_pretty(&summary)
        .map_err(|e| e.to_string())
        .and_then(|bytes| {
            std::fs::write(case.directory.join("w4_case.json"), bytes).map_err(|e| e.to_string())
        });
    match result {
        Err(error) => Err(format!(
            "{error}；原进程收尾={cleanup:?}；原采集={final_capture_error:?}；证据保存={saved:?}"
        )),
        Ok(mut evidence) => {
            cleanup?;
            saved?;
            if let Some(error) = final_capture_error {
                return Err(error);
            }
            evidence["capture"] = capture_evidence;
            evidence["final_state"] = final_state?;
            evidence["exit_code"] = json!(0);
            Ok(evidence)
        }
    }
}

async fn close_child(
    child: &mut Child,
    mut capture: Option<&mut TraceCapture>,
    mut check_reply: impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    let mut reply_error = check_reply().err();
    match child.try_wait() {
        Ok(Some(status)) => {
            if status.code() != Some(0) {
                return Err(format!("原进程已退出且非code0：{status}"));
            }
            return reply_error.map_or(Ok(()), Err);
        }
        Ok(None) => {}
        Err(error) => {
            if reply_error.is_none() {
                reply_error = Some(format!("查询原Child退出失败：{error}"));
            }
        }
    }
    let normal = {
        let stopping = stop(child);
        tokio::pin!(stopping);
        loop {
            tokio::select! {
                result = &mut stopping => break result,
                () = tokio::time::sleep(Duration::from_millis(2)) => {
                    if let Some(capture) = &mut capture { capture.drain(); }
                    if let Err(error) = check_reply() && reply_error.is_none() {
                        reply_error = Some(error);
                    }
                }
            }
        }
    };
    if let Err(error) = check_reply()
        && reply_error.is_none()
    {
        reply_error = Some(error);
    }
    if let Ok(Some(status)) = child.try_wait() {
        normal?;
        if status.code() != Some(0) {
            return Err(format!("原收尾实际非code0：{status}"));
        }
        return reply_error.map_or(Ok(()), Err);
    }
    let kill = child.start_kill().map_err(|e| e.to_string());
    let until = SystemClock.now().0.saturating_add(2_000_000_000);
    loop {
        if let Some(capture) = &mut capture {
            capture.drain();
        }
        if matches!(child.try_wait(), Ok(Some(_))) {
            return Err(format!("原正常关闭={normal:?}；强制回收={kill:?}"));
        }
        if SystemClock.now().0 >= until {
            // 原句柄未回收时终止验收父进程，不能以Drop原Child伪装已完成清理。
            std::process::abort();
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "原请求、采样和Child在同一借用作用域保留至真实收尾，错误不能提前析构原RR"
)]
async fn exercise(
    case: &Case,
    child: &mut Child,
    port: &ControlPort,
    capture: &mut TraceCapture,
    mode: Mode,
) -> Result<Value, String> {
    let mut owned_node = None;
    let mut owned_client = None;
    let mut owned_pending = None;
    let mut reply_count = 0_u8;
    let mut latch_probe = LatchProbe {
        client: None,
        pending: None,
        replies: 0,
    };
    // 从创建第一件资源开始就保原件；只有真实成功才安装，所有Err都先共同真reap。
    let result = async {
        wait_log(case, child, "s0", "boot_ready", Duration::from_secs(12)).await?;
        let (sample, _) =
            send_sample_request(port, case, child, &capture.page, SampleMode::ArmInflight).await?;
        capture.sample = Some(sample);
        capture.drain();
        owned_node = Some(
            NodeBuilder::new()
                .create::<Service>()
                .map_err(|e| e.to_string())?,
        );
        let node = owned_node.as_ref().ok_or("原Node安装后丢失")?;
        let profile = robot_transport::iceoryx2::RrProfile::S0_CONTROL;
        owned_client = Some(
            robot_transport::iceoryx2::rr_client_with::<
                markers::ClearEStopRequest,
                markers::ClearEStopResponse,
            >(
                node,
                &format!("/{}/control/clear_estop", case.robot_id),
                &profile,
            )
            .map_err(|e| e.to_string())?,
        );
        let client = owned_client.as_ref().ok_or("原Clear client安装后丢失")?;
        let request_id = format!("{}-clear", case.robot_id);
        let started = SystemClock.now().0;
        let body = robot_msg::build_helpers::build_clear_estop_request(
            &request_id,
            &request_id,
            "W4a",
            msg::RpcRole::Maintainer,
            "确认真实停止",
            "核原Created与Unsynced修复链",
            false,
            started.saturating_add(5_000_000_000),
            started,
        );
        std::fs::write(case.directory.join("clear_request.bin"), &body)
            .map_err(|e| e.to_string())?;
        owned_pending = Some(client.send_typed(&body).map_err(|e| format!("{e:?}"))?);
        let pending = owned_pending.as_ref().ok_or("原Clear pending安装后丢失")?;
        // 已入队 issue 不能回滚成无原件错误；原 Pending 仍陪同 Child 真 reap。
        if let Some(issue) = pending.delivery_issue() {
            return Err(format!("原Clear已入队但附带投递问题：{issue:?}"));
        }
        let until = started.saturating_add(REPLY_WAIT_NS);
        let reply = loop {
            capture.drain();
            if let Some(bytes) = receive_with_closed_tail(pending)? {
                reply_count = 1;
                break bytes;
            }
            require_running(child, until, "原Clear退休回复")?;
            tokio::time::sleep(Duration::from_millis(2)).await;
        };
        std::fs::write(case.directory.join("clear_response.bin"), &reply)
            .map_err(|e| e.to_string())?;
        verify_clear_reply(&reply, &request_id)?;
        let chain = loop {
            capture.drain();
            match capture.validate(mode) {
                Ok(chain) => break chain,
                Err(error) if SystemClock.now().0 >= until => return Err(error),
                Err(error) => {
                    if let Err(wait) = require_running(child, until, "原writer命令末事件") {
                        return Err(format!("{error}；原等待={wait}"));
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        };
        // 完整CommandEnd/Token/Completion已验证；此时才能结束原样本。
        disarm_with_drain(port, case, child, capture).await?;
        capture.drain();
        let _ = capture.validate(mode)?;
        verify_files(case)?;
        let latch = verify_latch_read_only(case, child, node, &mut latch_probe).await?;
        Ok(
            json!({"request_id": request_id, "sample_seq": sample, "chain": chain,
        "original_failure": "FsyncFailed { stage: SyncMarkerDirectory }",
        "retired": true, "clear_reply_count": 1, "latch_probe": latch,
        "latch_pair": [1,1], "marker_present": false, "directory": case.directory}),
        )
    }
    .await;
    let cleanup = close_child(child, Some(capture), || {
        let clear: Result<(), String> = (|| {
            if let Some(pending) = &owned_pending
                && let Some(bytes) = receive_with_closed_tail(pending)?
            {
                let path = case
                    .directory
                    .join(format!("clear_cleanup_response_{reply_count}.bin"));
                std::fs::write(path, &bytes).map_err(|e| e.to_string())?;
                reply_count = reply_count.checked_add(1).ok_or("原Clear响应计数溢出")?;
                if reply_count != 1 {
                    return Err("收尾时原Clear请求出现第二份响应".into());
                }
            }
            Ok(())
        })();
        // 分别观察两份原pending，不能因一份报错跳过另一份的尾回复保存。
        let probe = latch_probe.poll_cleanup(case);
        clear?;
        probe
    })
    .await;
    match result {
        Ok(evidence) => {
            cleanup?;
            Ok(evidence)
        }
        Err(error) => Err(format!("{error}；保原RR句柄的进程收尾={cleanup:?}")),
    }
}

async fn disarm_with_drain(
    port: &ControlPort,
    case: &Case,
    child: &mut Child,
    capture: &mut TraceCapture,
) -> Result<(), String> {
    // 只重开同一原页的只读边界视图；原环仍由capture唯一消费，不新建页或采样代次。
    let boundary_page = case.trace("s0")?;
    let disarming = send_sample_request(port, case, child, &boundary_page, SampleMode::Disarm);
    tokio::pin!(disarming);
    loop {
        tokio::select! {
            result = &mut disarming => { let _ = result?; return Ok(()); }
            () = tokio::time::sleep(Duration::from_millis(2)) => capture.drain(),
        }
    }
}

fn require_running(child: &mut Child, until: u64, label: &str) -> Result<(), String> {
    if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
        return Err(format!("等待{label}时原进程提前退出：{status}"));
    }
    if SystemClock.now().0 >= until {
        return Err(format!("等待{label}超过原观察截止"));
    }
    Ok(())
}

fn verify_clear_reply(bytes: &[u8], request_id: &str) -> Result<(), String> {
    let response =
        flatbuffers::root::<msg::ClearEStopResponse>(bytes).map_err(|e| e.to_string())?;
    if response.request_id() != Some(request_id)
        || response.status() != msg::RpcStatus::Internal
        || response.domain() != msg::ClearEStopStatus::FsyncFailed
        || response.error_msg() != Some("FsyncFailed { stage: SyncMarkerDirectory }")
        || !response.retired()
        || response.dry_run()
        || response.superseded()
        || response.cleared_at_ns() != 0
        || response.effect() != msg::SideEffect::Started
    {
        return Err(format!("原首失败/退休响应不符：{response:?}"));
    }
    Ok(())
}

fn verify_files(case: &Case) -> Result<(), String> {
    for path in [&case.latch, &case.directory.join("estop_latch.bak")] {
        if std::fs::read(path).map_err(|e| e.to_string())? != b"1\n" {
            return Err(format!("原修复目标未保持1/1：{}", path.display()));
        }
    }
    match std::fs::symlink_metadata(case.directory.join("estop_latch.clear_pending")) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("不能证明marker不存在：{error}")),
        Ok(_) => Err("原marker仍存在".into()),
    }
}

/// 原client先安装，真实send成功才安装pending；失败也保这两件到共同真reap。
struct LatchProbe {
    client: Option<Iceoryx2RrClient<markers::SetEnabledRequest, markers::SetEnabledResponse>>,
    pending: Option<Iceoryx2PendingResponse<markers::SetEnabledResponse>>,
    replies: u8,
}

impl LatchProbe {
    fn poll_cleanup(&mut self, case: &Case) -> Result<(), String> {
        if let Some(pending) = &self.pending
            && let Some(bytes) = receive_with_closed_tail(pending)?
        {
            let path = case
                .directory
                .join(format!("latch_probe_cleanup_response_{}.bin", self.replies));
            std::fs::write(path, &bytes).map_err(|e| e.to_string())?;
            self.replies = self.replies.checked_add(1).ok_or("只读探针响应计数溢出")?;
            if self.replies != 1 {
                return Err("只读探针产生第二份响应".into());
            }
        }
        Ok(())
    }
}

async fn verify_latch_read_only(
    case: &Case,
    child: &mut Child,
    node: &iceoryx2::node::Node<Service>,
    probe: &mut LatchProbe,
) -> Result<Value, String> {
    let before = case.observation("s0")?.serial_calls[0];
    let request_id = format!("{}-latch-probe", case.robot_id);
    let now = SystemClock.now().0;
    let body = robot_msg::build_helpers::build_set_enabled_request(
        &request_id,
        &request_id,
        "W4a只读核锁存",
        msg::RpcRole::Maintainer,
        true,
        true,
        now.saturating_add(REPLY_WAIT_NS),
        "不执行Enable",
        now,
    );
    std::fs::write(case.directory.join("latch_probe_request.bin"), &body)
        .map_err(|e| e.to_string())?;
    probe.client = Some(
        robot_transport::iceoryx2::rr_client_with::<
            markers::SetEnabledRequest,
            markers::SetEnabledResponse,
        >(
            node,
            &format!("/{}/control/set_enabled", case.robot_id),
            &robot_transport::iceoryx2::RrProfile::S0_CONTROL,
        )
        .map_err(|e| e.to_string())?,
    );
    let client = probe.client.as_ref().ok_or("原只读client安装后丢失")?;
    probe.pending = Some(client.send_typed(&body).map_err(|e| format!("{e:?}"))?);
    let pending = probe.pending.as_ref().ok_or("原只读pending安装后丢失")?;
    if let Some(issue) = pending.delivery_issue() {
        return Err(format!("原只读探针已入队但附带投递问题：{issue:?}"));
    }
    let bytes = loop {
        if let Some(bytes) = receive_with_closed_tail(pending)? {
            probe.replies = 1;
            break bytes;
        }
        require_running(child, now.saturating_add(REPLY_WAIT_NS), "原只读latch评估")?;
        tokio::time::sleep(Duration::from_millis(2)).await;
    };
    std::fs::write(case.directory.join("latch_probe_response.bin"), &bytes)
        .map_err(|e| e.to_string())?;
    let response =
        flatbuffers::root::<msg::SetEnabledResponse>(&bytes).map_err(|e| e.to_string())?;
    let after = case.observation("s0")?.serial_calls[0];
    if response.request_id() != Some(request_id.as_str())
        || response.status() != msg::RpcStatus::FailedPrecondition
        || response.domain() != msg::SetEnabledStatus::LatchBlocked
        || response.effect() != msg::SideEffect::None
        || !response.retired()
        || response.completed_at_ns() != 0
        || before != after
    {
        return Err(format!(
            "原latch只读证据不符：{response:?}；normal_calls={before}/{after}"
        ));
    }
    Ok(
        json!({"request_id": request_id, "domain": "LatchBlocked", "effect": "None",
        "normal_calls_before": before, "normal_calls_after": after}),
    )
}

fn receive_with_closed_tail<T: markers::BodyMarker>(
    pending: &Iceoryx2PendingResponse<T>,
) -> Result<Option<Vec<u8>>, String> {
    let received = pending.try_recv().map_err(|e| e.to_string())?;
    match (received, pending.connection_state()) {
        // 只有原连接正常 Closed 才建立尾发布可见性，随后补收一次。
        (None, StrictChannelStatus::Closed) => pending.try_recv().map_err(|e| e.to_string()),
        // Open/Unknown 不构成退休证据；两份原 Pending 仍保留到共同真 reap。
        (received, _) => Ok(received),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_rejects_zero_unknown_and_incomplete_scope() {
        assert!(analyze_trace(&[], Mode::Created).is_err());
        // 畸形观测输入只测解析拒绝，不造writer凭证或把它称为真实进程证据。
        let event = TraceEvent {
            sample_seq: 1,
            stamp_ns: 1,
            kind: robot_s0::comm_fault_trace::TraceKind::W4CommandBegin,
            source: 1,
            normal_seq: 1,
            generation: u32::MAX,
            a: 1,
            b: 1,
        };
        assert!(analyze_trace(&[event], Mode::Created).is_err());
        let txn = ClearTxnIdentity {
            txn_id: 1_u128 << 100,
            expected_epoch: 7,
            original_latched: true,
        };
        let mut payloads = vec![
            W4Payload::CommandBegin {
                txn,
                command: CommandKind::RemoveRepaired,
            },
            W4Payload::Token {
                after: false,
                token: TokenKind::NoMarker,
            },
            W4Payload::Completion {
                kind: CompletionKind::RemovedRepaired,
                failure: None,
            },
            W4Payload::Token {
                after: true,
                token: TokenKind::None,
            },
            W4Payload::CommandEnd { completed: true },
        ];
        let parsed = parse_command(1, &payloads).expect("只解析本地观测结构");
        assert_eq!(parsed.txn, txn);
        payloads.insert(3, payloads[2]);
        assert!(parse_command(1, &payloads).is_err());
        payloads.remove(3);
        let _ = payloads.pop();
        assert!(parse_command(1, &payloads).is_err());
    }

    #[test]
    fn parser_requires_alias_identity_then_real_bad_fd_return() {
        let stage = ClearTxnStage::SyncMarkerDirectory;
        let mut payloads = vec![
            W4Payload::Stage {
                succeeded: false,
                step_seq: 1,
                stage,
                occurrence: 1,
            },
            W4Payload::FaultSelected {
                slot: 0,
                step_seq: 1,
                stage,
                occurrence: 1,
            },
            W4Payload::Alias {
                verified: false,
                slot: 0,
                step_seq: 1,
                dev: 3,
                ino: 4,
            },
            W4Payload::Alias {
                verified: true,
                slot: 0,
                step_seq: 1,
                dev: 3,
                ino: 5,
            },
            W4Payload::DirSyncCall {
                slot: 0,
                step_seq: 1,
            },
            W4Payload::DirSyncReturned {
                slot: 0,
                step_seq: 1,
                success: false,
                errno: Some(libc::EBADF),
            },
        ];
        let parse = |values: &[W4Payload]| {
            parse_step(
                &mut PayloadCursor {
                    payloads: values,
                    index: 0,
                },
                1,
                &mut [0; 2],
            )
        };
        assert!(parse(&payloads).is_err());
        payloads[3] = W4Payload::Alias {
            verified: true,
            slot: 0,
            step_seq: 1,
            dev: 3,
            ino: 4,
        };
        assert!(!parse(&payloads).expect("只解析本地观测结构").succeeded);
        payloads[5] = W4Payload::DirSyncReturned {
            slot: 0,
            step_seq: 1,
            success: false,
            errno: Some(libc::EIO),
        };
        assert!(parse(&payloads).is_err());
        payloads.remove(4);
        assert!(parse(&payloads).is_err());
    }
}
