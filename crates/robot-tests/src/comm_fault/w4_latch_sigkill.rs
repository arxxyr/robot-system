//! W4b 五个真实成功步骤后的进程中断；同内核重启不等于断电耐久验证。

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::ExitStatus;
use std::time::Duration;

use iceoryx2::node::Node;
use iceoryx2::prelude::NodeBuilder;
use iceoryx2::service::ipc::Service;
use iceoryx2::strict::StrictChannelStatus;
use robot_core::time::{RobotClock, SystemClock};
use robot_msg::{markers, msg};
use robot_s0::clear_estop_transaction::{ClearTxnStage, ClearTxnStore};
use robot_s0::comm_fault_trace::w4::{CommandKind, CompletionKind, TokenKind, W4Payload};
use robot_s0::comm_fault_trace::{SampleMode, TraceEvent, TracePage, w4};
use robot_s0::latch_writer::ClearTxnIdentity;
use robot_transport::iceoryx2::request_response::{Iceoryx2PendingResponse, Iceoryx2RrClient};
use serde_json::{Value, json};
use tokio::process::Child;

use super::Args;
use super::control::ControlPort;
use super::harness::{Case, stop};
use super::report::{Report, file_hash};
use super::sla::send_sample_request;

const FILTER: &str = "W4-FILE-SIGKILL-SMOKE";
const DAEMON_LOG: &str = "warn,s0_daemon=info";
const STARTUP_NS: u64 = 12_000_000_000;
const REQUEST_NS: u64 = 5_000_000_000;
const REAP_NS: u64 = 2_000_000_000;
const STEP_NS: u64 = 2_000_000_000;
const PAUSE_NS: u64 = 250_000_000;
const MAX_EVENTS: usize = 1024;
const MAX_SCOPES: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Point {
    CreateMarker,
    RenameBackup,
    PrepareSync,
    RemoveMarker,
    RemovalSync,
}

impl Point {
    const ALL: [Self; 5] = [
        Self::CreateMarker,
        Self::RenameBackup,
        Self::PrepareSync,
        Self::RemoveMarker,
        Self::RemovalSync,
    ];
    const fn name(self) -> &'static str {
        match self {
            Self::CreateMarker => "kill_after_create_marker_refuses_empty_marker",
            Self::RenameBackup => "kill_after_backup_rename_repairs_mixed_pair",
            Self::PrepareSync => "kill_after_prepare_last_sync_keeps_latch_on_restart",
            Self::RemoveMarker => "kill_after_unlink_restarts_from_real_zero_pair",
            Self::RemovalSync => "kill_after_removal_sync_restarts_from_real_zero_pair",
        }
    }
    const fn configuration(self) -> &'static str {
        match self {
            Self::CreateMarker => "prepare-create-marker",
            Self::RenameBackup => "prepare-rename-backup",
            Self::PrepareSync => "prepare-sync-primary-directory",
            Self::RemoveMarker => "finalize-remove-marker",
            Self::RemovalSync => "finalize-sync-removal-directory",
        }
    }
    const fn stage(self) -> ClearTxnStage {
        match self {
            Self::CreateMarker => ClearTxnStage::CreateMarker,
            Self::RenameBackup => ClearTxnStage::RenameBackup,
            Self::PrepareSync => ClearTxnStage::SyncPrimaryDirectory,
            Self::RemoveMarker => ClearTxnStage::RemoveMarker,
            Self::RemovalSync => ClearTxnStage::SyncRemovalDirectory,
        }
    }
    const fn finalize(self) -> bool {
        matches!(self, Self::RemoveMarker | Self::RemovalSync)
    }
    const fn latch_after_restart(self) -> bool {
        !self.finalize()
    }
}

#[derive(Clone, Copy)]
enum Invocation {
    Crash,
    Restart,
}

fn invocation_case(shared: &Case, which: Invocation) -> Case {
    let name = match which {
        Invocation::Crash => "crash",
        Invocation::Restart => "restart",
    };
    Case {
        directory: shared.directory.join("invocations").join(name),
        config: shared.config.clone(),
        robot_id: shared.robot_id.clone(),
        latch: shared.latch.clone(),
        trust: shared.trust.clone(),
        bin_dir: shared.bin_dir.clone(),
    }
}

fn create_file(path: &Path) -> Result<File, String> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| format!("新证据 {}：{e}", path.display()))
}
fn save_bytes(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = create_file(path)?;
    file.write_all(bytes)
        .and_then(|()| file.flush())
        .map_err(|e| e.to_string())
}
fn save_json(path: &Path, value: &Value) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
    save_bytes(path, &bytes)
}
fn append_json(file: &mut BufWriter<File>, value: &Value) -> Result<(), String> {
    serde_json::to_writer(&mut *file, value).map_err(|e| e.to_string())?;
    file.write_all(b"\n")
        .and_then(|()| file.flush())
        .map_err(|e| e.to_string())
}
fn remember(first: &mut Option<String>, result: Result<(), String>) {
    if let Err(error) = result {
        first.get_or_insert(error);
    }
}
fn combine<T>(result: Result<T, String>, cleanup: Result<(), String>) -> Result<T, String> {
    match (result, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(error)) | (Err(error), Ok(())) => Err(error),
        (Err(error), Err(cleanup)) => Err(format!("{error}；原件收尾另报={cleanup}")),
    }
}

pub(super) async fn run(args: &Args, report: &mut Report) -> Result<(), String> {
    if !cfg!(target_os = "linux") || args.case.as_deref() != Some(FILTER) {
        return Err("W4b 仅支持 Linux 的明确五例筛选，拒绝零例通过".into());
    }
    let count = report.cases.len();
    for point in Point::ALL {
        let started = SystemClock.now().0;
        let result = exercise_one(args, point).await;
        report.record(point.name(), FILTER, started, result);
    }
    if report.cases.len().checked_sub(count) != Some(5) {
        return Err("W4b 实际登记不是五例".into());
    }
    Ok(())
}

#[derive(Clone, Copy, Debug)]
struct Reached {
    source: u64,
    step_seq: u64,
    stage: ClearTxnStage,
    stamp: u64,
    entered_at: u64,
    pause_until: u64,
}
impl Reached {
    fn from_event(event: TraceEvent, payload: W4Payload) -> Option<Self> {
        match payload {
            W4Payload::KillReached {
                step_seq,
                stage,
                entered_at,
                pause_until,
            } => Some(Self {
                source: event.source,
                step_seq,
                stage,
                stamp: event.stamp_ns,
                entered_at,
                pause_until,
            }),
            _ => None,
        }
    }
    fn check(self, armed: u64, before: u64, after: u64) -> Result<(), String> {
        let deadline = self
            .entered_at
            .checked_add(STEP_NS)
            .ok_or("原FileStep截止溢出")?;
        if armed == 0
            || self.entered_at < armed
            || self.entered_at > self.stamp
            || self.stamp >= self.pause_until
            || self
                .pause_until
                .checked_sub(self.stamp)
                .is_none_or(|n| n > PAUSE_NS)
            || self.pause_until >= deadline
            || before < self.stamp
            || after < before
            || before >= self.pause_until
            || after >= self.pause_until
            || before >= deadline
            || after >= deadline
        {
            return Err(format!(
                "Reached或实际信号不在原固定窗口：{self:?}，armed={armed}，kill={before}/{after}"
            ));
        }
        Ok(())
    }
}

struct Capture {
    page: TracePage,
    csv: BufWriter<File>,
    events: Vec<(TraceEvent, W4Payload)>,
    sample: Option<u64>,
    scopes: usize,
    total: u64,
    first: Option<String>,
}
impl Capture {
    fn open(case: &Case) -> Result<Self, String> {
        let page = case.trace("s0")?;
        let mut csv = BufWriter::new(create_file(&case.directory.join("ordered_trace.csv"))?);
        csv.write_all(b"sample,stamp,kind,source,normal_seq,generation,a,b\n")
            .map_err(|e| e.to_string())?;
        Ok(Self {
            page,
            csv,
            events: Vec::new(),
            sample: None,
            scopes: 0,
            total: 0,
            first: None,
        })
    }
    fn drain(&mut self) {
        for event in self.page.drain(robot_s0::comm_fault_trace::TRACE_CAPACITY) {
            remember(
                &mut self.first,
                writeln!(
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
                )
                .map_err(|e| e.to_string()),
            );
            match self.total.checked_add(1) {
                Some(n) => self.total = n,
                None => {
                    self.first.get_or_insert("原trace计数溢出".into());
                }
            }
            if self
                .sample
                .is_some_and(|sample| sample != event.sample_seq || event.sample_seq == 0)
            {
                self.first
                    .get_or_insert("事件不属于当前已确认sample".into());
            }
            let decoded = w4::decode(&event);
            match decoded {
                Ok(Some(payload)) => {
                    if self.sample != Some(event.sample_seq) || event.sample_seq == 0 {
                        self.first.get_or_insert("W4事件不属于原已确认样本".into());
                    }
                    if matches!(payload, W4Payload::CommandBegin { .. }) {
                        match self.scopes.checked_add(1) {
                            Some(n) => self.scopes = n,
                            None => {
                                self.first.get_or_insert("原scope计数溢出".into());
                            }
                        }
                    }
                    if self.events.len() >= MAX_EVENTS || self.scopes > MAX_SCOPES {
                        self.first.get_or_insert("W4内存证据超过原有界容量".into());
                    } else {
                        self.events.push((event, payload));
                    }
                }
                Ok(None) => (),
                Err(error) => {
                    self.first
                        .get_or_insert(format!("W4未知/无效事件：{error}"));
                }
            }
        }
        if self.page.dropped() != 0 {
            self.first
                .get_or_insert(format!("原trace丢失{}", self.page.dropped()));
        }
        remember(&mut self.first, self.csv.flush().map_err(|e| e.to_string()));
    }
    fn check(&self) -> Result<(), String> {
        self.first.clone().map_or(Ok(()), Err)
    }
    fn reached(&self) -> Result<Option<Reached>, String> {
        self.check()?;
        let mut found = None;
        for &(event, payload) in &self.events {
            if let Some(value) = Reached::from_event(event, payload)
                && found.replace(value).is_some()
            {
                return Err("同次样本多次Reached".into());
            }
            if matches!(payload, W4Payload::Failure { .. }) {
                return Err(format!("真实hook夹具失败：{payload:?}"));
            }
        }
        Ok(found)
    }
    fn terminal(&mut self, case: &Case, status: ExitStatus, disarmed: bool) -> Result<(), String> {
        self.drain();
        let boundary = self.page.boundary_snapshot();
        save_json(
            &case.directory.join("terminal_boundary.json"),
            &json!({"at_ns":SystemClock.now().0,"signal":status.signal(),"code":status.code(),"interrupted":self.sample.is_some() && !disarmed,
            "successful_disarm":disarmed,"boundary":format!("{boundary:?}"),"sample":self.sample,"events":self.total,"w4_events":self.events.len(),"dropped":self.page.dropped(),"first_error":self.first}),
        )?;
        self.check()
    }
}

const PREPARE: [ClearTxnStage; 15] = [
    ClearTxnStage::CreateMarker,
    ClearTxnStage::SampleMarkerIdentity,
    ClearTxnStage::WriteMarker,
    ClearTxnStage::SyncMarkerFile,
    ClearTxnStage::SyncMarkerDirectory,
    ClearTxnStage::CreateBackup,
    ClearTxnStage::WriteBackup,
    ClearTxnStage::SyncBackupFile,
    ClearTxnStage::RenameBackup,
    ClearTxnStage::SyncBackupDirectory,
    ClearTxnStage::CreatePrimary,
    ClearTxnStage::WritePrimary,
    ClearTxnStage::SyncPrimaryFile,
    ClearTxnStage::RenamePrimary,
    ClearTxnStage::SyncPrimaryDirectory,
];
const FINALIZE: [ClearTxnStage; 4] = [
    ClearTxnStage::ReadMarker,
    ClearTxnStage::ValidateMarker,
    ClearTxnStage::RemoveMarker,
    ClearTxnStage::SyncRemovalDirectory,
];

struct Prefix<'a> {
    events: &'a [(TraceEvent, W4Payload)],
    at: usize,
    sample: u64,
    stamp: u64,
}
impl Prefix<'_> {
    fn next(&mut self, source: u64) -> Result<(TraceEvent, W4Payload), String> {
        let (event, payload) = *self.events.get(self.at).ok_or("原命令/步骤前缀缺失")?;
        self.at += 1;
        if event.sample_seq != self.sample
            || event.source != source
            || event.stamp_ns < self.stamp
            || self.sample == 0
            || source == 0
        {
            return Err("原sample/source/时间顺序不符".into());
        }
        self.stamp = event.stamp_ns;
        Ok((event, payload))
    }
    fn expect(&mut self, source: u64, expected: W4Payload) -> Result<(), String> {
        let (_, actual) = self.next(source)?;
        if actual != expected {
            return Err(format!("前缀实际={actual:?}，期望={expected:?}"));
        }
        Ok(())
    }
    fn begin(
        &mut self,
        source: u64,
        command: CommandKind,
        before: TokenKind,
        txn: Option<ClearTxnIdentity>,
    ) -> Result<ClearTxnIdentity, String> {
        let (_, payload) = self.next(source)?;
        let W4Payload::CommandBegin {
            txn: actual,
            command: actual_command,
        } = payload
        else {
            return Err("缺原CommandBegin".into());
        };
        if actual_command != command
            || !actual.original_latched
            || actual.txn_id == 0
            || txn.is_some_and(|t| t != actual)
        {
            return Err("原完整txn/command/original_latch不符".into());
        }
        self.expect(
            source,
            W4Payload::Token {
                after: false,
                token: before,
            },
        )?;
        Ok(actual)
    }
    fn steps(
        &mut self,
        source: u64,
        stages: &[ClearTxnStage],
        target: ClearTxnStage,
    ) -> Result<(), String> {
        for (index, &stage) in stages.iter().enumerate() {
            let step_seq = u64::try_from(index)
                .map_err(|e| e.to_string())?
                .checked_add(1)
                .ok_or("步骤序号溢出")?;
            let (_, before) = self.next(source)?;
            let W4Payload::Stage {
                succeeded: false,
                step_seq: actual,
                stage: actual_stage,
                occurrence,
            } = before
            else {
                return Err("缺原Stage before".into());
            };
            if actual != step_seq
                || actual_stage != stage
                || occurrence != u64::from(stage == target)
            {
                return Err("真实步骤次序/连续序号不符".into());
            }
            self.expect(
                source,
                W4Payload::Stage {
                    succeeded: true,
                    step_seq,
                    stage,
                    occurrence,
                },
            )?;
        }
        Ok(())
    }
}

fn validate_killed_prefix(
    events: &[(TraceEvent, W4Payload)],
    point: Point,
    sample: u64,
    armed: u64,
    status: ExitStatus,
    kill_before: u64,
    kill_after: u64,
) -> Result<Value, String> {
    if status.signal() != Some(libc::SIGKILL) {
        return Err("原Child未以SIGKILL真正reap".into());
    }
    if events.len() > MAX_EVENTS {
        return Err("解析事件超过原有界容量".into());
    }
    let source = events.first().ok_or("W4零事件")?.0.source;
    let mut cursor = Prefix {
        events,
        at: 0,
        sample,
        stamp: 0,
    };
    let txn = cursor.begin(source, CommandKind::Prepare, TokenKind::None, None)?;
    let target_source;
    let count;
    if point.finalize() {
        cursor.steps(source, &PREPARE, point.stage())?;
        cursor.expect(
            source,
            W4Payload::Completion {
                kind: CompletionKind::Prepared,
                failure: None,
            },
        )?;
        cursor.expect(
            source,
            W4Payload::Token {
                after: true,
                token: TokenKind::Prepared,
            },
        )?;
        cursor.expect(source, W4Payload::CommandEnd { completed: true })?;
        target_source = source.checked_add(1).ok_or("原command序号溢出")?;
        let _ = cursor.begin(
            target_source,
            CommandKind::RemoveCommitted,
            TokenKind::Prepared,
            Some(txn),
        )?;
        count = FINALIZE
            .iter()
            .position(|&s| s == point.stage())
            .ok_or("未知Finalize杀点")?
            + 1;
        cursor.steps(target_source, &FINALIZE[..count], point.stage())?;
    } else {
        target_source = source;
        count = PREPARE
            .iter()
            .position(|&s| s == point.stage())
            .ok_or("未知Prepare杀点")?
            + 1;
        cursor.steps(source, &PREPARE[..count], point.stage())?;
    }
    let (event, payload) = cursor.next(target_source)?;
    let reached = Reached::from_event(event, payload).ok_or("目标成功后缺唯一Reached")?;
    if reached.stage != point.stage()
        || reached.step_seq != u64::try_from(count).map_err(|e| e.to_string())?
        || cursor.at != events.len()
    {
        return Err("杀点不符或原截断后仍有下一步骤/Completion/Token/End".into());
    }
    reached.check(armed, kill_before, kill_after)?;
    Ok(
        json!({"txn_id":txn.txn_id.to_string(),"expected_epoch":txn.expected_epoch,"original_latched":txn.original_latched,
        "sample":sample,"source":reached.source,"step_seq":reached.step_seq,"stage":format!("{:?}",reached.stage),"events":events.len(),"first_rr_retired":false}),
    )
}

/// 取得原对象后即装入外层槽；任何创建、落证据、send或解析错误都先回收Child。
#[derive(Default)]
struct Owned {
    child: Option<Child>,
    port: Option<ControlPort>,
    node: Option<Node<Service>>,
    clear_client: Option<Iceoryx2RrClient<markers::ClearEStopRequest, markers::ClearEStopResponse>>,
    clear_pending: Option<Iceoryx2PendingResponse<markers::ClearEStopResponse>>,
    probe_client: Option<Iceoryx2RrClient<markers::SetEnabledRequest, markers::SetEnabledResponse>>,
    probe_pending: Option<Iceoryx2PendingResponse<markers::SetEnabledResponse>>,
    capture: Option<Capture>,
    clear_log: Option<BufWriter<File>>,
    probe_log: Option<BufWriter<File>>,
    clear_replies: usize,
    probe_replies: usize,
    disarmed: bool,
    kill_sent: bool,
}
impl Owned {
    fn drain(&mut self) {
        if let Some(capture) = &mut self.capture {
            capture.drain();
        }
    }
    fn poll_clear(&mut self, case: &Case, strict: bool) -> Result<(), String> {
        let Some(pending) = &self.clear_pending else {
            return Ok(());
        };
        let now = SystemClock.now().0;
        let (channel_state, polled) = receive_with_closed_tail(pending);
        let mut error = None;
        match polled {
            Ok(Some(bytes)) => {
                self.clear_replies = self
                    .clear_replies
                    .checked_add(1)
                    .ok_or("原Clear回复计数溢出")?;
                remember(
                    &mut error,
                    save_bytes(
                        &case
                            .directory
                            .join(format!("clear_response_{}.bin", self.clear_replies)),
                        &bytes,
                    ),
                );
                if let Some(log) = &mut self.clear_log {
                    remember(
                        &mut error,
                        append_json(
                            log,
                            &json!({"at_ns":now,"result":"response","index":self.clear_replies,"bytes":bytes.len(),"channel_state":format!("{channel_state:?}")}),
                        ),
                    );
                }
                error
                    .get_or_insert("首次Clear在杀点/中断收尾出现业务回复，不能冒截断未退休".into());
            }
            Ok(None) => {
                if let Some(log) = &mut self.clear_log {
                    remember(
                        &mut error,
                        append_json(
                            log,
                            &json!({"at_ns":now,"result":"pending","channel_state":format!("{channel_state:?}")}),
                        ),
                    );
                }
            }
            Err(failure) => {
                let message = failure;
                if let Some(log) = &mut self.clear_log {
                    remember(
                        &mut error,
                        append_json(
                            log,
                            &json!({"at_ns":now,"result":"transport_error","error":message,"after_signal":!strict,"channel_state":format!("{channel_state:?}")}),
                        ),
                    );
                }
                if strict {
                    error.get_or_insert(format!("原Clear poll失败：{message}"));
                }
            }
        }
        error.map_or(Ok(()), Err)
    }
    fn poll_probe_tail(&mut self, case: &Case) -> Result<(), String> {
        let Some(pending) = &self.probe_pending else {
            return Ok(());
        };
        let (channel_state, received) = receive_with_closed_tail(pending);
        match received {
            Ok(None) => {
                if let Some(log) = &mut self.probe_log {
                    append_json(
                        log,
                        &json!({"at_ns":SystemClock.now().0,"result":"tail_pending","channel_state":format!("{channel_state:?}")}),
                    )?;
                }
                Ok(())
            }
            Ok(Some(bytes)) => {
                self.probe_replies = self
                    .probe_replies
                    .checked_add(1)
                    .ok_or("只读回复计数溢出")?;
                save_bytes(
                    &case
                        .directory
                        .join(format!("probe_response_{}.bin", self.probe_replies)),
                    &bytes,
                )?;
                if let Some(log) = &mut self.probe_log {
                    append_json(
                        log,
                        &json!({"at_ns":SystemClock.now().0,"result":"tail_response","index":self.probe_replies,"channel_state":format!("{channel_state:?}")}),
                    )?;
                }
                Err("第二次只读请求出现额外回复".into())
            }
            Err(error) => {
                if let Some(log) = &mut self.probe_log {
                    append_json(
                        log,
                        &json!({"at_ns":SystemClock.now().0,"result":"tail_transport_error","error":error,"channel_state":format!("{channel_state:?}")}),
                    )?;
                }
                Err(error)
            }
        }
    }
    fn poll_cleanup(&mut self, case: &Case) -> Result<(), String> {
        let a = self.poll_clear(case, false);
        let b = self.poll_probe_tail(case);
        combine(a, b)
    }
    fn running(&mut self, until: u64, label: &str) -> Result<(), String> {
        let child = self.child.as_mut().ok_or("原Child不存在")?;
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            return Err(format!("{label}前真实退出：{status}"));
        }
        if SystemClock.now().0 >= until {
            return Err(format!("{label}超过原父观察包络"));
        }
        Ok(())
    }
    async fn armed(&mut self, case: &Case, until: u64) -> Result<u64, String> {
        loop {
            self.drain();
            self.running(until, "启动/运行监督武装")?;
            let observation = case.observation("s0")?;
            let logs = case.logs("s0")?;
            if logs.contains("boot_ready") && observation.runtime_armed_ns != 0 {
                save_json(
                    &case.directory.join("armed.json"),
                    &json!({"at_ns":SystemClock.now().0,"observation":format!("{observation:?}")}),
                )?;
                return Ok(observation.runtime_armed_ns);
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }
    async fn sample(&mut self, case: &Case, mode: SampleMode) -> Result<u64, String> {
        let boundary = case.trace("s0")?;
        let port = self.port.as_ref().ok_or("原ControlPort未安装")?;
        let child = self.child.as_mut().ok_or("原Child未安装")?;
        let capture = self.capture.as_mut().ok_or("原Capture未安装")?;
        let request = send_sample_request(port, case, child, &boundary, mode);
        tokio::pin!(request);
        let (sequence, _) = loop {
            tokio::select! {
                result=&mut request => break result?,
                ()=tokio::time::sleep(Duration::from_millis(2)) => capture.drain(),
            }
        };
        match mode {
            SampleMode::ArmInflight => capture.sample = Some(sequence),
            SampleMode::Disarm => self.disarmed = true,
            SampleMode::ArmIdle => return Err("W4b不能以ArmIdle替换原ArmInflight".into()),
        }
        Ok(sequence)
    }
    fn send_clear(&mut self, case: &Case) -> Result<u64, String> {
        self.node = Some(
            NodeBuilder::new()
                .create::<Service>()
                .map_err(|e| format!("{e:?}"))?,
        );
        self.clear_client = Some(
            robot_transport::iceoryx2::rr_client_with::<
                markers::ClearEStopRequest,
                markers::ClearEStopResponse,
            >(
                self.node.as_ref().ok_or("原node丢失")?,
                &format!("/{}/control/clear_estop", case.robot_id),
                &robot_transport::iceoryx2::RrProfile::S0_CONTROL,
            )
            .map_err(|e| e.to_string())?,
        );
        self.clear_log = Some(BufWriter::new(create_file(
            &case.directory.join("clear_poll.jsonl"),
        )?));
        let now = SystemClock.now().0;
        let deadline = now.checked_add(REQUEST_NS).ok_or("Clear期限溢出")?;
        let id = format!("{}-crash-clear", case.robot_id);
        let body = robot_msg::build_helpers::build_clear_estop_request(
            &id,
            &id,
            "W4b",
            msg::RpcRole::Maintainer,
            "确认真实停止",
            "真实成功步骤后中断",
            false,
            deadline,
            now,
        );
        save_bytes(&case.directory.join("clear_request.bin"), &body)?;
        self.clear_pending = Some(
            self.clear_client
                .as_ref()
                .ok_or("原Clear client丢失")?
                .send_typed(&body)
                .map_err(|e| format!("{e:?}"))?,
        );
        let pending = self
            .clear_pending
            .as_ref()
            .ok_or("原Clear pending安装后丢失")?;
        if let Some(log) = &mut self.clear_log {
            append_json(
                log,
                &json!({"at_ns":SystemClock.now().0,"result":"enqueued","delivery_issue":pending.delivery_issue().map(|issue|format!("{issue:?}"))}),
            )?;
        }
        // issue 属于真实已入队结果；报错前原件已安装，保留到 reap_owned 完成。
        if let Some(issue) = pending.delivery_issue() {
            return Err(format!("原Clear已入队但附带投递问题：{issue:?}"));
        }
        Ok(deadline)
    }
}

#[derive(Clone, Copy)]
enum ExpectedExit {
    Killed,
    Normal,
    Rejected,
}
impl ExpectedExit {
    fn accepts(self, status: ExitStatus) -> bool {
        match self {
            Self::Killed => status.signal() == Some(libc::SIGKILL),
            Self::Normal => status.code() == Some(0),
            Self::Rejected => status.code() == Some(1),
        }
    }
}

/// Child只暂移到本收尾栈；其它原对象仍在Owned，回收失败必须终止父进程而非detach。
async fn reap_owned(
    owned: &mut Owned,
    case: &Case,
    expected: ExpectedExit,
) -> Result<ExitStatus, String> {
    let Some(mut child) = owned.child.take() else {
        return Err("未启动原Child".into());
    };
    let mut first = None;
    let initial = child.try_wait().map_err(|e| e.to_string());
    let mut status = match initial {
        Ok(value) => value,
        Err(error) => {
            first = Some(error);
            None
        }
    };
    if status.is_none() && matches!(expected, ExpectedExit::Normal) {
        {
            let closing = stop(&mut child);
            tokio::pin!(closing);
            loop {
                tokio::select! {
                    result=&mut closing => {remember(&mut first,result);break;}
                    ()=tokio::time::sleep(Duration::from_millis(2)) => {owned.drain();remember(&mut first,owned.poll_cleanup(case));}
                }
            }
        }
        match child.try_wait() {
            Ok(value) => status = value,
            Err(error) => {
                first.get_or_insert(error.to_string());
            }
        }
    }
    // 预期SIGKILL已发；先保原2s回收窗，超界才进入失败清理。
    if status.is_none() && matches!(expected, ExpectedExit::Killed) && owned.kill_sent {
        let until = SystemClock.now().0.saturating_add(REAP_NS);
        loop {
            owned.drain();
            remember(&mut first, owned.poll_cleanup(case));
            match child.try_wait() {
                Ok(Some(value)) => {
                    status = Some(value);
                    break;
                }
                Ok(None) => (),
                Err(error) => {
                    first.get_or_insert(error.to_string());
                    break;
                }
            }
            if SystemClock.now().0 >= until {
                first.get_or_insert("原SIGKILL回收超过2s".into());
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }
    if status.is_none() {
        first.get_or_insert("原预期退出未完成，执行失败收尾".into());
        remember(&mut first, child.start_kill().map_err(|e| e.to_string()));
        let until = SystemClock.now().0.saturating_add(REAP_NS);
        loop {
            owned.drain();
            remember(&mut first, owned.poll_cleanup(case));
            match child.try_wait() {
                Ok(Some(value)) => {
                    status = Some(value);
                    break;
                }
                Ok(None) => (),
                Err(error) => {
                    first.get_or_insert(error.to_string());
                }
            }
            if SystemClock.now().0 >= until {
                let _ = save_json(
                    &case.directory.join("reap_failure.json"),
                    &json!({"at_ns":SystemClock.now().0,"error":first,"child_id":child.id()}),
                );
                std::process::abort();
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }
    owned.child = Some(child);
    let status = status.ok_or("回收状态缺失")?;
    owned.drain();
    remember(&mut first, owned.poll_cleanup(case));
    let exit_evidence = json!({"at_ns":SystemClock.now().0,"code":status.code(),"signal":status.signal(),"reaped":true,"first_error":first.clone()});
    remember(
        &mut first,
        save_json(&case.directory.join("exit.json"), &exit_evidence),
    );
    if let Some(capture) = &mut owned.capture {
        remember(&mut first, capture.terminal(case, status, owned.disarmed));
    }
    if !expected.accepts(status) {
        first.get_or_insert(format!("真实退出不符：{status}"));
    }
    first.map_or(Ok(status), Err)
}

async fn wait_after_success_and_kill_owned(
    owned: &mut Owned,
    case: &Case,
    point: Point,
    armed: u64,
    deadline: u64,
) -> Result<(u64, u64), String> {
    let reached = loop {
        owned.drain();
        owned.poll_clear(case, true)?;
        owned.running(deadline, "唯一成功步骤Reached")?;
        if let Some(reached) = owned.capture.as_ref().ok_or("原capture丢失")?.reached()? {
            break reached;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    };
    if reached.stage != point.stage() {
        return Err("真实Reached不是本例目标阶段".into());
    }
    owned.poll_clear(case, true)?;
    let child = owned.child.as_mut().ok_or("原Child丢失")?;
    if child.try_wait().map_err(|e| e.to_string())?.is_some() {
        return Err("信号前原Child已经退出".into());
    }
    let pid =
        i32::try_from(child.id().ok_or("信号前原Child没有当前id")?).map_err(|e| e.to_string())?;
    if pid <= 0 {
        return Err("禁止向非正PID发信号".into());
    }
    let before = SystemClock.now().0;
    reached.check(armed, before, before)?;
    // 仅向刚从同一存活Child取得的正PID发送；返回和实际回收分别记录。
    let result = unsafe { libc::kill(pid, libc::SIGKILL) };
    let errno = if result == 0 {
        None
    } else {
        std::io::Error::last_os_error().raw_os_error()
    };
    let after = SystemClock.now().0;
    owned.kill_sent = result == 0;
    save_json(
        &case.directory.join("kill.json"),
        &json!({"pid":pid,"signal":libc::SIGKILL,"before_ns":before,"after_ns":after,
        "return":result,"errno":errno,"sample":owned.capture.as_ref().and_then(|c|c.sample),"source":reached.source,"step_seq":reached.step_seq,
        "stage":format!("{:?}",reached.stage),"entered_at":reached.entered_at,"reached_stamp":reached.stamp,"pause_until":reached.pause_until,"armed_at":armed}),
    )?;
    if result != 0 {
        return Err(format!("原SIGKILL失败：errno={errno:?}"));
    }
    reached.check(armed, before, after)?;
    Ok((before, after))
}

fn record_child_identity(case: &Case, owned: &Owned) -> Result<(), String> {
    let pid = owned
        .child
        .as_ref()
        .and_then(Child::id)
        .ok_or("刚安装的原Child缺当前PID")?;
    if pid == 0 {
        return Err("原Child PID不得为0".into());
    }
    save_json(
        &case.directory.join("child_identity.json"),
        &json!({"at_ns":SystemClock.now().0,"pid":pid,"rust_log":DAEMON_LOG}),
    )
}

fn original_child_pid(case: &Case) -> Result<u32, String> {
    let bytes =
        std::fs::read(case.directory.join("child_identity.json")).map_err(|e| e.to_string())?;
    let identity: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    let pid = u32::try_from(identity["pid"].as_u64().ok_or("原Child证据缺真实PID")?)
        .map_err(|e| e.to_string())?;
    if pid == 0 {
        return Err("原Child证据PID为0".into());
    }
    Ok(pid)
}

async fn first_invocation(case: &Case, point: Point) -> Result<Value, String> {
    let mut owned = Owned::default();
    let result: Result<(u64,u64,u64,u64),String>=async {
        save_json(&case.directory.join("invocation.json"),&json!({"kind":"crash","rust_log":DAEMON_LOG,"configuration":point.configuration(),"config":case.config,"latch":case.latch,"trust":case.trust,"robot_id":case.robot_id}))?;
        owned.port=Some(ControlPort::new(&case.robot_id)?);
        let mut command=case.command();command.env("ROBOT_TEST_W4_FILE_KILL",point.configuration()).env("ROBOT_LOG_FORMAT","json").env("RUST_LOG",DAEMON_LOG);
        let started=SystemClock.now().0;
        owned.child=Some(case.spawn("s0",command)?);
        record_child_identity(case, &owned)?;
        owned.capture=Some(Capture::open(case)?);
        let armed=owned.armed(case,started.saturating_add(STARTUP_NS)).await?;
        let sample=owned.sample(case,SampleMode::ArmInflight).await?;
        let deadline=owned.send_clear(case)?;
        let (before,after)=wait_after_success_and_kill_owned(&mut owned,case,point,armed,deadline).await?;
        Ok((armed,sample,before,after))
    }.await;
    let cleanup = match owned.child {
        Some(_) => reap_owned(&mut owned, case, ExpectedExit::Killed).await,
        None => Err("第一invocation未产生Child".into()),
    };
    let result = match (result, cleanup) {
        (Ok((armed, sample, before, after)), Ok(status)) => (|| {
            let capture = owned.capture.as_ref().ok_or("真reap后Capture缺失")?;
            capture.check()?;
            validate_killed_prefix(&capture.events, point, sample, armed, status, before, after)
        })(),
        (Err(error), cleanup) => Err(format!("{error}；原Child收尾={cleanup:?}")),
        (Ok(_), Err(error)) => Err(error),
    };
    if let Err(error) = &result {
        let _ = save_json(
            &case.directory.join("failure.json"),
            &json!({"at_ns":SystemClock.now().0,"error":error}),
        );
    }
    // 本函数返回才析构所有原RR与注入端点；调用方随后才能冻结首轮目录并重启。
    result
}

async fn probe_latch(owned: &mut Owned, case: &Case, latched: bool) -> Result<Value, String> {
    owned.node = Some(
        NodeBuilder::new()
            .create::<Service>()
            .map_err(|e| format!("{e:?}"))?,
    );
    owned.probe_client = Some(
        robot_transport::iceoryx2::rr_client_with::<
            markers::SetEnabledRequest,
            markers::SetEnabledResponse,
        >(
            owned.node.as_ref().ok_or("第二原node丢失")?,
            &format!("/{}/control/set_enabled", case.robot_id),
            &robot_transport::iceoryx2::RrProfile::S0_CONTROL,
        )
        .map_err(|e| e.to_string())?,
    );
    owned.probe_log = Some(BufWriter::new(create_file(
        &case.directory.join("probe_poll.jsonl"),
    )?));
    let before = case.observation("s0")?.serial_calls[0];
    let started = SystemClock.now().0;
    let until = started.checked_add(REQUEST_NS).ok_or("只读请求截止溢出")?;
    let id = format!("{}-restart-readonly", case.robot_id);
    let body = robot_msg::build_helpers::build_set_enabled_request(
        &id,
        &id,
        "W4b重启只读",
        msg::RpcRole::Maintainer,
        true,
        true,
        until,
        "不执行Enable",
        started,
    );
    save_bytes(&case.directory.join("probe_request.bin"), &body)?;
    owned.probe_pending = Some(
        owned
            .probe_client
            .as_ref()
            .ok_or("第二原client丢失")?
            .send_typed(&body)
            .map_err(|e| format!("{e:?}"))?,
    );
    let pending = owned
        .probe_pending
        .as_ref()
        .ok_or("第二原pending安装后丢失")?;
    if let Some(log) = &mut owned.probe_log {
        append_json(
            log,
            &json!({"at_ns":SystemClock.now().0,"result":"enqueued","delivery_issue":pending.delivery_issue().map(|issue|format!("{issue:?}"))}),
        )?;
    }
    if let Some(issue) = pending.delivery_issue() {
        return Err(format!("第二只读探针已入队但附带投递问题：{issue:?}"));
    }
    let bytes = loop {
        owned.drain();
        let pending = owned.probe_pending.as_ref().ok_or("第二原pending丢失")?;
        let (channel_state, received) = receive_with_closed_tail(pending);
        if let Some(log) = &mut owned.probe_log {
            append_json(
                log,
                &json!({"at_ns":SystemClock.now().0,"result":match &received {Ok(Some(_))=>"response",Ok(None)=>"pending",Err(_)=>"transport_error"},"error":received.as_ref().err(),"channel_state":format!("{channel_state:?}")}),
            )?;
        }
        if let Some(bytes) = received? {
            owned.probe_replies = 1;
            break bytes;
        }
        owned.running(until, "第二次只读评估")?;
        tokio::time::sleep(Duration::from_millis(2)).await;
    };
    save_bytes(&case.directory.join("probe_response_1.bin"), &bytes)?;
    let response =
        flatbuffers::root::<msg::SetEnabledResponse>(&bytes).map_err(|e| e.to_string())?;
    let after = case.observation("s0")?.serial_calls[0];
    validate_probe(&response, &id, latched, before, after)?;
    Ok(
        json!({"request_id":id,"latch_expected":latched,"status":format!("{:?}",response.status()),"domain":format!("{:?}",response.domain()),
        "error_msg":response.error_msg(),"completed_at_ns":response.completed_at_ns(),"effect":"None","retired":response.retired(),"normal_before":before,"normal_after":after}),
    )
}

fn receive_with_closed_tail<T: markers::BodyMarker>(
    pending: &Iceoryx2PendingResponse<T>,
) -> (StrictChannelStatus, Result<Option<Vec<u8>>, String>) {
    let received = pending.try_recv().map_err(|e| e.to_string());
    let state = pending.connection_state();
    let received = match (received, state) {
        // 正常关闭的可见性只用于补收；Unknown/Open 继续由原 Child 收尾托底。
        (Ok(None), StrictChannelStatus::Closed) => pending.try_recv().map_err(|e| e.to_string()),
        (received, _) => received,
    };
    (state, received)
}

fn validate_probe(
    response: &msg::SetEnabledResponse<'_>,
    id: &str,
    latched: bool,
    before: u64,
    after: u64,
) -> Result<(), String> {
    if response.request_id() != Some(id)
        || response.effect() != msg::SideEffect::None
        || !response.retired()
        || before != after
    {
        return Err(format!(
            "只读原id/effect/退休或normal调用数不符：{response:?}，{before}/{after}"
        ));
    }
    let accepted = match (latched, response.status(), response.domain()) {
        (true, msg::RpcStatus::FailedPrecondition, msg::SetEnabledStatus::LatchBlocked) => {
            response.completed_at_ns() == 0
        }
        (false, msg::RpcStatus::Ok, msg::SetEnabledStatus::WouldAccept) => {
            response.completed_at_ns() != 0
        }
        (false, msg::RpcStatus::FailedPrecondition, msg::SetEnabledStatus::GuardRejected) => {
            response.completed_at_ns() == 0
                && matches!(
                    response.error_msg(),
                    Some(
                        "set_enabled precondition rejected; state=Safe"
                            | "set_enabled precondition rejected; state=Ready"
                    )
                )
        }
        _ => false,
    };
    if !accepted {
        return Err(format!("真实内存latch只读oracle不符：{response:?}"));
    }
    Ok(())
}

fn initial_latch(logs: &str) -> Result<bool, String> {
    let mut values = Vec::new();
    for line in logs.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value["message"] == "initial estop_latched" {
            values.push(
                value["latched"]
                    .as_bool()
                    .ok_or("真实初始latch日志缺bool")?,
            );
        }
    }
    match values.as_slice() {
        [latched] => Ok(*latched),
        _ => Err(format!(
            "真实initial estop_latched日志不是唯一一条：{values:?}"
        )),
    }
}

fn verify_initial_latch(logs: &str, expected: bool) -> Result<(), String> {
    if initial_latch(logs)? != expected {
        return Err("实际启动日志latch与恢复oracle不符".into());
    }
    Ok(())
}

async fn wait_rejected(owned: &mut Owned, case: &Case, until: u64) -> Result<(), String> {
    loop {
        owned.drain();
        if let Some(status) = owned
            .child
            .as_mut()
            .ok_or("重启原Child丢失")?
            .try_wait()
            .map_err(|e| e.to_string())?
        {
            if status.code() != Some(1) {
                return Err(format!("坏marker真实启动退出不是1：{status}"));
            }
            let observation = case.observation("s0")?;
            let logs = case.logs("s0")?;
            if observation.ready_ns != 0
                || observation.ready_reached
                || observation.first_io_ns != 0
                || observation.serial_calls != [0; 6]
                || observation.runtime_armed_ns != 0
                || observation.fired_ns != 0
                || logs.contains("boot_ready")
                || !logs.contains("ValidateMarker")
            {
                return Err(format!(
                    "空marker未沿原ValidateMarker在IO前拒绝：{observation:?}"
                ));
            }
            save_json(
                &case.directory.join("probe_not_started.json"),
                &json!({"reason":"原ValidateMarker拒绝，未创建RR端点","observation":format!("{observation:?}")}),
            )?;
            return Ok(());
        }
        if SystemClock.now().0 >= until {
            return Err("坏marker重启未在原12s父包络退出".into());
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

async fn restart_same_directory(case: &Case, point: Point) -> Result<Value, String> {
    let mut owned = Owned::default();
    let result: Result<Value,String>=async {
        save_json(&case.directory.join("invocation.json"),&json!({"kind":"restart","rust_log":DAEMON_LOG,"file_kill_configured":false,"file_fault_configured":false,"config":case.config,"latch":case.latch,"trust":case.trust,"robot_id":case.robot_id}))?;
        let mut command=case.command();command.env("ROBOT_LOG_FORMAT","json").env("RUST_LOG",DAEMON_LOG);
        let started=SystemClock.now().0;
        owned.child=Some(case.spawn("s0",command)?);
        record_child_identity(case, &owned)?;
        owned.capture=Some(Capture::open(case)?);
        match point {
            Point::CreateMarker=>{
                wait_rejected(&mut owned,case,started.saturating_add(STARTUP_NS)).await?;
                Ok(json!({"recovery":"ValidateMarker rejected","second_rr_started":false}))
            }
            Point::RenameBackup|Point::PrepareSync|Point::RemoveMarker|Point::RemovalSync=>{
                let _=owned.armed(case,started.saturating_add(STARTUP_NS)).await?;
                verify_initial_latch(&case.logs("s0")?, point.latch_after_restart())?;
                owned.port=Some(ControlPort::new(&case.robot_id)?);
                let sample=owned.sample(case,SampleMode::ArmInflight).await?;
                let probe=probe_latch(&mut owned,case,point.latch_after_restart()).await?;
                let _=owned.sample(case,SampleMode::Disarm).await?;
                Ok(json!({"initial_latched":point.latch_after_restart(),"sample":sample,"probe":probe}))
            }
        }
    }.await;
    let expected = match point {
        Point::CreateMarker => ExpectedExit::Rejected,
        _ => ExpectedExit::Normal,
    };
    let cleanup = if owned.child.is_some() {
        reap_owned(&mut owned, case, expected).await.map(|_| ())
    } else {
        Err("第二invocation未产生Child".into())
    };
    let result = combine(result, cleanup).and_then(|evidence| {
        let observation = case.observation("s0")?;
        if observation.fired_ns != 0 || observation.residual_bitmap != 0 {
            return Err(format!("第二次退出有监督/保护残件：{observation:?}"));
        }
        if let Some(capture) = &owned.capture {
            capture.check()?;
            if !capture.events.is_empty() {
                return Err("第二次无配置重启仍产生W4事务事件".into());
            }
        }
        Ok(evidence)
    });
    if let Err(error) = &result {
        let _ = save_json(
            &case.directory.join("failure.json"),
            &json!({"at_ns":SystemClock.now().0,"error":error}),
        );
    }
    result
}

fn path_identity(path: &Path, bytes: bool) -> Result<Value, String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(json!({"present":false}));
        }
        Err(error) => return Err(error.to_string()),
    };
    if metadata.file_type().is_symlink() {
        return Err(format!("原持久对象变成symlink：{}", path.display()));
    }
    let mut result = json!({"present":true,"dev":metadata.dev(),"ino":metadata.ino(),"len":metadata.len(),"directory":metadata.is_dir(),"nlink":metadata.nlink()});
    if bytes && metadata.is_file() {
        result["sha256"] = json!(file_hash(path)?);
        result["bytes"] = json!(std::fs::read(path).map_err(|e| e.to_string())?);
    }
    Ok(result)
}

fn persistent_snapshot(shared: &Case) -> Result<Value, String> {
    let directory = shared.latch.parent().ok_or("原latch没有父目录")?;
    let mut files = BTreeMap::new();
    for entry in std::fs::read_dir(directory).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "持久文件名非UTF8")?;
        if name.starts_with("estop_latch") && name != "estop_latch.lock" {
            files.insert(name, path_identity(&entry.path(), true)?);
        }
    }
    Ok(
        json!({"directory":path_identity(directory,false)?,"lock":path_identity(&directory.join("estop_latch.lock"),false)?,
        "config":path_identity(&shared.config,true)?,"trust":path_identity(&shared.trust,false)?,"trust_files":hash_tree(&shared.trust)?,"trust_lock":path_identity(&shared.trust.join(robot_s0::rx_trust::LOCK_NAME),true)?,"files":files}),
    )
}

fn verify_killed_files(shared: &Case, point: Point, chain: &Value) -> Result<(), String> {
    let directory = shared.latch.parent().ok_or("原持久目录丢失")?;
    let expected = match point {
        Point::CreateMarker => (b"1\n".as_slice(), b"1\n".as_slice()),
        Point::RenameBackup => (b"1\n".as_slice(), b"0\n".as_slice()),
        _ => (b"0\n".as_slice(), b"0\n".as_slice()),
    };
    if std::fs::read(&shared.latch).map_err(|e| e.to_string())? != expected.0
        || std::fs::read(directory.join("estop_latch.bak")).map_err(|e| e.to_string())?
            != expected.1
    {
        return Err("真实杀后双份状态不符".into());
    }
    let mut store =
        ClearTxnStore::open(&shared.latch, &shared.robot_id, "arm").map_err(|e| e.to_string())?;
    match point {
        Point::CreateMarker => {
            let marker = std::fs::metadata(store.marker_path()).map_err(|e| e.to_string())?;
            if !marker.is_file() || marker.len() != 0 {
                return Err("CreateMarker杀后不是原空普通marker".into());
            }
        }
        Point::RenameBackup | Point::PrepareSync => {
            // 仅调用原只读校验；不获得/制造Prepared，不运行recover或repair。
            let pending = store
                .inspect_pending()
                .map_err(|e| e.to_string())?
                .ok_or("有效marker丢失")?;
            let record = pending.record();
            if record.txn_id().to_string() != chain["txn_id"].as_str().ok_or("原txn证据丢失")?
                || !record.original_latched()
                || Some(record.expected_epoch()) != chain["expected_epoch"].as_u64()
            {
                return Err("真实有效marker不属于首次原txn".into());
            }
        }
        Point::RemoveMarker | Point::RemovalSync => {
            match std::fs::symlink_metadata(store.marker_path()) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                Err(error) => return Err(error.to_string()),
                Ok(_) => return Err("原unlink后marker仍存在".into()),
            }
        }
    }
    Ok(())
}

/// 只允许同原锁在真实两次取锁后写对应Child PID；其它trust数据仍完整比对。
fn verify_trust_lock(
    before: &Value,
    after: &Value,
    expected_pids: (u32, u32),
) -> Result<(), String> {
    let name = robot_s0::rx_trust::LOCK_NAME;
    let old = before["trust_files"]
        .as_object()
        .ok_or("首轮trust文件集合缺失")?;
    let new = after["trust_files"]
        .as_object()
        .ok_or("次轮trust文件集合缺失")?;
    if !old.keys().eq(new.keys()) || !old.contains_key(name) {
        return Err("trust完整文件集合发生变化或缺原锁".into());
    }
    for (file, hash) in old {
        if file != name && new.get(file) != Some(hash) {
            return Err(format!("原trust数据字节变化：{file}"));
        }
    }
    for key in ["present", "dev", "ino", "directory", "nlink"] {
        if before["trust_lock"][key] != after["trust_lock"][key] {
            return Err(format!("原rx_trust锁身份变化：{key}"));
        }
    }
    for (snapshot, pid) in [(before, expected_pids.0), (after, expected_pids.1)] {
        let lock = &snapshot["trust_lock"];
        if pid == 0
            || lock["present"] != true
            || lock["directory"] != false
            || lock["nlink"] != 1
            || lock["bytes"] != json!(format!("{pid}\n").into_bytes())
            || snapshot["trust_files"][name] != lock["sha256"]
        {
            return Err("原rx_trust锁不是同轮真实Child PID或快照不一致".into());
        }
    }
    Ok(())
}

fn verify_restarted_files(
    shared: &Case,
    point: Point,
    before: &Value,
    after: &Value,
    expected_pids: (u32, u32),
) -> Result<(), String> {
    verify_trust_lock(before, after, expected_pids)?;
    for field in ["config", "trust"] {
        if before[field] != after[field] {
            return Err(format!("原共享{field}身份/字节被替换"));
        }
    }
    for object in ["directory", "lock"] {
        for key in ["present", "dev", "ino"] {
            if before[object][key] != after[object][key] {
                return Err(format!("原{object} {key}变化"));
            }
        }
    }
    if !before["lock"]["present"].as_bool().unwrap_or(false) {
        return Err("第一回收后原稳定锁不存在".into());
    }
    #[allow(
        clippy::single_match_else,
        reason = "按杀点枚举区分空标记拒绝启动和其余真实恢复，保留业务枚举分派"
    )]
    match point {
        Point::CreateMarker => {
            if before["files"] != after["files"] {
                return Err("空marker拒绝启动仍改动原双份/marker字节或inode".into());
            }
        }
        _ => {
            let directory = shared.latch.parent().ok_or("原持久目录丢失")?;
            let expected = if point.latch_after_restart() {
                b"1\n"
            } else {
                b"0\n"
            };
            for path in [&shared.latch, &directory.join("estop_latch.bak")] {
                if std::fs::read(path).map_err(|e| e.to_string())? != expected {
                    return Err("第二原生产恢复的双份不符".into());
                }
            }
            if after["files"].get("estop_latch.clear_pending").is_some() {
                return Err("第二恢复后marker未移除".into());
            }
        }
    }
    Ok(())
}

fn hash_tree(root: &Path) -> Result<BTreeMap<String, String>, String> {
    fn visit(
        root: &Path,
        current: &Path,
        files: &mut BTreeMap<String, String>,
    ) -> Result<(), String> {
        for entry in std::fs::read_dir(current).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let kind = entry.file_type().map_err(|e| e.to_string())?;
            if kind.is_dir() {
                visit(root, &entry.path(), files)?;
            } else if kind.is_file() {
                let path = entry.path();
                let key = path
                    .strip_prefix(root)
                    .map_err(|e| e.to_string())?
                    .to_str()
                    .ok_or("证据路径非UTF8")?
                    .to_owned();
                files.insert(key, file_hash(&path)?);
            } else {
                return Err(format!("证据集合含非普通文件：{}", entry.path().display()));
            }
        }
        Ok(())
    }
    let mut files = BTreeMap::new();
    visit(root, root, &mut files)?;
    Ok(files)
}

async fn exercise_one(args: &Args, point: Point) -> Result<Value, String> {
    let shared = Case::new(&args.evidence_dir, &args.bin_dir, point.name(), 1)?;
    let directory = shared.latch.parent().ok_or("原Case没有持久目录")?;
    for path in [&shared.latch, &directory.join("estop_latch.bak")] {
        std::fs::write(path, b"1\n").map_err(|e| e.to_string())?;
    }
    let invocations = shared.directory.join("invocations");
    std::fs::create_dir(&invocations).map_err(|e| e.to_string())?;
    let crash = invocation_case(&shared, Invocation::Crash);
    let restart = invocation_case(&shared, Invocation::Restart);
    for case in [&crash, &restart] {
        std::fs::create_dir(&case.directory).map_err(|e| e.to_string())?;
    }
    let first = first_invocation(&crash, point).await;
    // 首轮原对象已全部结束作用域；冻结错误不得覆盖原业务/收尾首错。
    let frozen = (|| {
        let manifest = hash_tree(&crash.directory)?;
        save_json(
            &shared.directory.join("first_invocation_manifest.json"),
            &json!(manifest),
        )?;
        Ok::<_, String>(manifest)
    })();
    let chain = combine(first, frozen.as_ref().map(|_| ()).map_err(Clone::clone))?;
    let manifest = frozen?;
    verify_killed_files(&shared, point, &chain)?;
    let before = persistent_snapshot(&shared)?;
    save_json(
        &shared.directory.join("persistent_before_restart.json"),
        &before,
    )?;
    let second = restart_same_directory(&restart, point).await;
    // 两项独立读取均执行；即使第二业务失败也留下完整首轮哈希复核。
    let after = persistent_snapshot(&shared);
    let current = hash_tree(&crash.directory);
    let mut evidence_error = None;
    match after {
        Ok(snapshot) => {
            remember(
                &mut evidence_error,
                save_json(
                    &shared.directory.join("persistent_after_restart.json"),
                    &snapshot,
                ),
            );
            remember(
                &mut evidence_error,
                original_child_pid(&crash).and_then(|first| {
                    original_child_pid(&restart).and_then(|second| {
                        verify_restarted_files(&shared, point, &before, &snapshot, (first, second))
                    })
                }),
            );
        }
        Err(error) => {
            evidence_error.get_or_insert(error);
        }
    }
    match current {
        Ok(current) => {
            remember(
                &mut evidence_error,
                save_json(
                    &shared
                        .directory
                        .join("first_invocation_manifest_after_restart.json"),
                    &json!(current),
                ),
            );
            if current != manifest {
                evidence_error
                    .get_or_insert("第二invocation改变了第一全部证据的路径集合或SHA".into());
            }
        }
        Err(error) => {
            evidence_error.get_or_insert(error);
        }
    }
    let second = combine(second, evidence_error.map_or(Ok(()), Err))?;
    Ok(
        json!({"point":point.configuration(),"first":chain,"second":second,"directory":shared.directory,
        "first_manifest_unchanged":true,"first_files":manifest.len(),"first_rr_retired":false,"same_kernel_process_interruption_only":true}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use robot_s0::comm_fault_trace::TraceKind;

    // 这里只构造已解码的诊断输入和Unix状态值，验证解析器；不冒真实进程、凭证或SIGKILL证据。
    #[allow(
        clippy::too_many_lines,
        reason = "同一诊断夹具连续列出Prepare及Finalize原事件顺序，避免拆散截断前缀"
    )]
    fn prefix_fixture(point: Point) -> Vec<(TraceEvent, W4Payload)> {
        let txn = ClearTxnIdentity {
            txn_id: (7_u128 << 96) | 0x13,
            expected_epoch: 23,
            original_latched: true,
        };
        let mut events = Vec::new();
        let mut push = |source: u64, payload: W4Payload| {
            let stamp_ns = 1_000 + u64::try_from(events.len()).unwrap();
            events.push((
                TraceEvent {
                    sample_seq: 7,
                    stamp_ns,
                    kind: TraceKind::W4CommandBegin,
                    source,
                    normal_seq: 0,
                    generation: 0,
                    a: 0,
                    b: 0,
                },
                payload,
            ));
        };
        push(
            11,
            W4Payload::CommandBegin {
                txn,
                command: CommandKind::Prepare,
            },
        );
        push(
            11,
            W4Payload::Token {
                after: false,
                token: TokenKind::None,
            },
        );
        let first_count = if point.finalize() {
            PREPARE.len()
        } else {
            PREPARE.iter().position(|&s| s == point.stage()).unwrap() + 1
        };
        for (index, &stage) in PREPARE[..first_count].iter().enumerate() {
            for succeeded in [false, true] {
                push(
                    11,
                    W4Payload::Stage {
                        succeeded,
                        step_seq: u64::try_from(index + 1).unwrap(),
                        stage,
                        occurrence: u64::from(stage == point.stage()),
                    },
                );
            }
        }
        let (source, count) = match point {
            Point::RemoveMarker | Point::RemovalSync => {
                push(
                    11,
                    W4Payload::Completion {
                        kind: CompletionKind::Prepared,
                        failure: None,
                    },
                );
                push(
                    11,
                    W4Payload::Token {
                        after: true,
                        token: TokenKind::Prepared,
                    },
                );
                push(11, W4Payload::CommandEnd { completed: true });
                push(
                    12,
                    W4Payload::CommandBegin {
                        txn,
                        command: CommandKind::RemoveCommitted,
                    },
                );
                push(
                    12,
                    W4Payload::Token {
                        after: false,
                        token: TokenKind::Prepared,
                    },
                );
                let count = FINALIZE.iter().position(|&s| s == point.stage()).unwrap() + 1;
                for (index, &stage) in FINALIZE[..count].iter().enumerate() {
                    for succeeded in [false, true] {
                        push(
                            12,
                            W4Payload::Stage {
                                succeeded,
                                step_seq: u64::try_from(index + 1).unwrap(),
                                stage,
                                occurrence: u64::from(stage == point.stage()),
                            },
                        );
                    }
                }
                (12, count)
            }
            _ => (11, first_count),
        };
        push(
            source,
            W4Payload::KillReached {
                step_seq: u64::try_from(count).unwrap(),
                stage: point.stage(),
                entered_at: 1_000,
                pause_until: 1_000_000,
            },
        );
        events
    }
    fn validate_fixture(events: &[(TraceEvent, W4Payload)], point: Point) -> Result<Value, String> {
        validate_killed_prefix(
            events,
            point,
            7,
            500,
            ExitStatus::from_raw(libc::SIGKILL),
            10_000,
            10_001,
        )
    }

    #[test]
    fn five_exact_prefixes_include_only_real_completed_predecessors() {
        for point in Point::ALL {
            assert!(
                validate_fixture(&prefix_fixture(point), point).is_ok(),
                "{point:?}"
            );
        }
        let create = prefix_fixture(Point::CreateMarker);
        assert_eq!(create.len(), 5);
        assert!(!create.iter().any(|(_, p)| matches!(
            p,
            W4Payload::Completion { .. }
                | W4Payload::CommandEnd { .. }
                | W4Payload::Token { after: true, .. }
        )));
    }

    #[test]
    fn killed_prefix_rejects_zero_missing_step_pair_and_extra_tail() {
        assert!(validate_fixture(&[], Point::CreateMarker).is_err());
        for point in Point::ALL {
            let mut missing = prefix_fixture(point);
            missing.drain(2..4);
            assert!(validate_fixture(&missing, point).is_err());
            let mut tail = prefix_fixture(point);
            let event = tail.last().unwrap().0;
            tail.push((event, W4Payload::CommandEnd { completed: true }));
            assert!(validate_fixture(&tail, point).is_err());
        }
        let mut large = prefix_fixture(Point::CreateMarker);
        large.resize(MAX_EVENTS + 1, large[0]);
        assert!(validate_fixture(&large, Point::CreateMarker).is_err());
    }

    #[test]
    fn killed_prefix_rejects_foreign_source_sample_txn_and_occurrence() {
        let point = Point::RemovalSync;
        let valid = prefix_fixture(point);
        let mut source = valid.clone();
        source[3].0.source += 1;
        assert!(validate_fixture(&source, point).is_err());
        let mut sample = valid.clone();
        sample[3].0.sample_seq = 0;
        assert!(validate_fixture(&sample, point).is_err());
        let mut txn = valid.clone();
        for (event, payload) in &mut txn {
            if event.source == 12
                && let W4Payload::CommandBegin { txn, .. } = payload
            {
                txn.txn_id ^= 1_u128 << 96;
            }
        }
        assert!(validate_fixture(&txn, point).is_err());
        let mut occurrence = valid;
        for (_, payload) in &mut occurrence {
            if let W4Payload::Stage {
                stage: ClearTxnStage::CreateMarker,
                occurrence,
                ..
            } = payload
            {
                *occurrence = 1;
            }
        }
        assert!(validate_fixture(&occurrence, point).is_err());
    }

    #[test]
    fn killed_prefix_rejects_wrong_command_completion_and_selected_stage() {
        let point = Point::RemoveMarker;
        let mut command = prefix_fixture(point);
        for (event, payload) in &mut command {
            if event.source == 12
                && let W4Payload::CommandBegin { command, .. } = payload
            {
                *command = CommandKind::RepairLatched;
            }
        }
        assert!(validate_fixture(&command, point).is_err());
        let mut completion = prefix_fixture(point);
        for (_, payload) in &mut completion {
            if let W4Payload::Completion { kind, .. } = payload {
                *kind = CompletionKind::PrepareDurable;
            }
        }
        assert!(validate_fixture(&completion, point).is_err());
        let mut stage = prefix_fixture(point);
        if let W4Payload::KillReached { stage, .. } = &mut stage.last_mut().unwrap().1 {
            *stage = ClearTxnStage::SyncRemovalDirectory;
        }
        assert!(validate_fixture(&stage, point).is_err());
    }

    #[test]
    fn reached_requires_actual_signal_and_original_bounded_clock_window() {
        let point = Point::CreateMarker;
        let events = prefix_fixture(point);
        for status in [ExitStatus::from_raw(0), ExitStatus::from_raw(libc::SIGTERM)] {
            assert!(
                validate_killed_prefix(&events, point, 7, 500, status, 10_000, 10_001).is_err()
            );
        }
        let reached =
            Reached::from_event(events.last().unwrap().0, events.last().unwrap().1).unwrap();
        assert!(reached.check(500, 10_000, 10_001).is_ok());
        for (armed, before, after) in [
            (0, 10_000, 10_001),
            (1_001, 10_000, 10_001),
            (500, 1, 10_001),
            (500, 10_001, 10_000),
            (500, 999_999, 1_000_000),
        ] {
            assert!(reached.check(armed, before, after).is_err());
        }
        assert!(
            Reached {
                entered_at: u64::MAX,
                ..reached
            }
            .check(1, 10_000, 10_001)
            .is_err()
        );
        assert!(
            Reached {
                pause_until: reached.stamp + PAUSE_NS + 1,
                ..reached
            }
            .check(500, 10_000, 10_001)
            .is_err()
        );
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "集中核同一只读回复的合法与拒绝矩阵，保留原状态和调用数断言的对应关系"
    )]
    fn restart_dry_run_accepts_only_exact_original_business_branches() {
        let check = |latch, status, domain, at, message: &str, before, after| {
            let bytes = robot_msg::build_helpers::build_set_enabled_response_typed(
                "original",
                status,
                domain,
                at,
                message,
                msg::SideEffect::None,
                true,
            );
            let response = flatbuffers::root::<msg::SetEnabledResponse>(&bytes).unwrap();
            validate_probe(&response, "original", latch, before, after)
        };
        assert!(
            check(
                true,
                msg::RpcStatus::FailedPrecondition,
                msg::SetEnabledStatus::LatchBlocked,
                0,
                "",
                0,
                0
            )
            .is_ok()
        );
        assert!(
            check(
                false,
                msg::RpcStatus::Ok,
                msg::SetEnabledStatus::WouldAccept,
                1,
                "",
                0,
                0
            )
            .is_ok()
        );
        for state in ["Safe", "Ready"] {
            let message = format!("set_enabled precondition rejected; state={state}");
            assert!(
                check(
                    false,
                    msg::RpcStatus::FailedPrecondition,
                    msg::SetEnabledStatus::GuardRejected,
                    0,
                    &message,
                    0,
                    0
                )
                .is_ok()
            );
        }
        for state in ["Fault", "Stopping", "Safe trailing", "Unknown"] {
            let message = format!("set_enabled precondition rejected; state={state}");
            assert!(
                check(
                    false,
                    msg::RpcStatus::FailedPrecondition,
                    msg::SetEnabledStatus::GuardRejected,
                    0,
                    &message,
                    0,
                    0
                )
                .is_err()
            );
        }
        assert!(
            check(
                false,
                msg::RpcStatus::FailedPrecondition,
                msg::SetEnabledStatus::LatchBlocked,
                0,
                "",
                0,
                0
            )
            .is_err()
        );
        assert!(
            check(
                false,
                msg::RpcStatus::Ok,
                msg::SetEnabledStatus::WouldAccept,
                0,
                "",
                0,
                0
            )
            .is_err()
        );
        assert!(
            check(
                false,
                msg::RpcStatus::Ok,
                msg::SetEnabledStatus::WouldAccept,
                1,
                "",
                0,
                1
            )
            .is_err()
        );
    }

    #[test]
    fn invocation_views_share_only_original_persistent_inputs() {
        let shared = Case {
            directory: "/original".into(),
            config: "/original/config.toml".into(),
            robot_id: "same-robot".into(),
            latch: "/original/estop_latch".into(),
            trust: "/original/trust".into(),
            bin_dir: "/bin-identity".into(),
        };
        let crash = invocation_case(&shared, Invocation::Crash);
        let restart = invocation_case(&shared, Invocation::Restart);
        assert_eq!(crash.directory, shared.directory.join("invocations/crash"));
        assert_eq!(
            restart.directory,
            shared.directory.join("invocations/restart")
        );
        for view in [crash, restart] {
            assert_eq!(view.config, shared.config);
            assert_eq!(view.latch, shared.latch);
            assert_eq!(view.trust, shared.trust);
            assert_eq!(view.robot_id, shared.robot_id);
            assert_eq!(view.bin_dir, shared.bin_dir);
        }
    }

    #[test]
    fn initial_latch_requires_unique_real_flattened_boolean_log() {
        let yes = r#"{"message":"initial estop_latched","latched":true}"#;
        let no = r#"{"message":"initial estop_latched","latched":false}"#;
        assert_eq!(initial_latch(yes), Ok(true));
        assert_eq!(initial_latch(no), Ok(false));
        assert!(verify_initial_latch(yes, true).is_ok());
        assert!(verify_initial_latch(no, true).is_err());
        assert!(verify_initial_latch(yes, false).is_err());
        assert!(verify_initial_latch("", true).is_err());
        assert!(initial_latch("").is_err());
        assert!(initial_latch(&format!("{yes}\n{no}")).is_err());
        assert!(initial_latch(r#"{"message":"initial estop_latched","latched":"true"}"#).is_err());
    }
    #[test]
    fn trust_pid_exception_requires_original_inode_exact_child_and_other_files() {
        // 纯证据比较反例；不伪造真实锁/进程的运行记录。
        let snapshot = |pid, hash: &str| {
            json!({
            "trust_files":{"rx_trust.lock":hash,"original.data":"unchanged"},
            "trust_lock":{"present":true,"directory":false,"dev":1,"ino":2,"nlink":1,
                "bytes":format!("{pid}\n").into_bytes(),"sha256":hash}})
        };
        let before = snapshot(41, "first");
        let after = snapshot(42, "second");
        assert!(verify_trust_lock(&before, &after, (41, 42)).is_ok());
        assert!(verify_trust_lock(&before, &after, (41, 43)).is_err());
        let mut changed = after.clone();
        changed["trust_lock"]["ino"] = json!(3);
        assert!(verify_trust_lock(&before, &changed, (41, 42)).is_err());
        let mut changed = after.clone();
        changed["trust_lock"]["bytes"] = json!(b"arbitrary\n".to_vec());
        assert!(verify_trust_lock(&before, &changed, (41, 42)).is_err());
        let mut changed = after.clone();
        changed["trust_files"]["original.data"] = json!("changed");
        assert!(verify_trust_lock(&before, &changed, (41, 42)).is_err());
        let mut changed = after.clone();
        changed["trust_files"]["extra"] = json!("new");
        assert!(verify_trust_lock(&before, &changed, (41, 42)).is_err());
        let mut changed = after;
        changed["trust_lock"]["sha256"] = json!("inconsistent");
        assert!(verify_trust_lock(&before, &changed, (41, 42)).is_err());
    }
}
