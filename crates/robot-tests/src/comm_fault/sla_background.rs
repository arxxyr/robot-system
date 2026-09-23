//! H2 的固定到达负载。64 是工具追踪上限，真实 wire 容量与拒绝语义保持不变。

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use robot_core::time::{RobotClock, SystemClock};
use robot_rpc::proto::robot_control_client::RobotControlClient;
use robot_rpc::proto::{Ack, CommandMeta, SetEnabledCommand};
use serde_json::{Value, json};
use tokio::task::{Id, JoinError, JoinSet};
use tonic::transport::Channel;

use super::rpc::command_meta;

pub(super) const RR_PERIOD_NS: u64 = 50_000_000;
pub(super) const RR_IN_FLIGHT_LIMIT: usize = 64;
const RR_TIMEOUT_NS: u64 = 2_000_000_000;
const RR_DRAIN_NS: u64 = 2_500_000_000;

#[derive(Debug)]
pub(super) struct BackgroundSchedule {
    next_due_ns: u64,
    failure: Option<String>,
}

impl BackgroundSchedule {
    pub(super) const fn new(first_due_ns: u64) -> Self {
        Self {
            next_due_ns: first_due_ns,
            failure: None,
        }
    }

    pub(super) fn admit(&mut self, now_ns: u64, tracked: usize) -> Result<Option<u64>, String> {
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        if now_ns < self.next_due_ns {
            return Ok(None);
        }
        let result = (|| {
            let next = self
                .next_due_ns
                .checked_add(RR_PERIOD_NS)
                .ok_or("背景 due 溢出")?;
            if now_ns >= next {
                return Err("背景请求启动迟到至少 50ms");
            }
            if tracked >= RR_IN_FLIGHT_LIMIT {
                return Err("背景追踪任务已达 64 上限");
            }
            let due = self.next_due_ns;
            self.next_due_ns = next;
            Ok(Some(due))
        })()
        .map_err(str::to_owned);
        if let Err(error) = &result {
            self.failure = Some(error.clone());
        }
        result
    }
}

#[derive(Default)]
struct Progress {
    requests: Vec<Value>,
    starts: Vec<Arc<AtomicU64>>,
    first_failure: Option<Value>,
    failures: Vec<Value>,
    peak_tracked: usize,
    admission_rejection: Option<Value>,
}

#[derive(Clone)]
pub(super) struct BackgroundHealth {
    progress: Arc<Mutex<Progress>>,
    stop_ns: Arc<AtomicU64>,
    run_until_ns: Arc<AtomicU64>,
    drained_ns: Arc<AtomicU64>,
    successful: Arc<AtomicU64>,
    wake: Arc<tokio::sync::Notify>,
    clock: Arc<dyn RobotClock>,
}

impl BackgroundHealth {
    fn new(clock: Arc<dyn RobotClock>) -> Self {
        Self {
            progress: Arc::new(Mutex::new(Progress::default())),
            stop_ns: Arc::new(AtomicU64::new(0)),
            run_until_ns: Arc::new(AtomicU64::new(0)),
            drained_ns: Arc::new(AtomicU64::new(0)),
            successful: Arc::new(AtomicU64::new(0)),
            wake: Arc::new(tokio::sync::Notify::new()),
            clock,
        }
    }

    pub(super) fn check(&self) -> Result<(), String> {
        let end = self.run_until_ns.load(Ordering::Acquire);
        if end != 0 && self.clock.now().0 >= end {
            self.stop();
        }
        match &self
            .progress
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .first_failure
        {
            Some(error) => Err(format!("背景负载首错：{error}")),
            None if self.stop_ns.load(Ordering::Acquire) != 0 => {
                Err("背景已停止，禁止新增恢复请求".to_owned())
            }
            None => Ok(()),
        }
    }

    fn fail(&self, error: impl Into<String>) {
        self.fail_from(&error.into(), &json!({"kind":"supervisor"}));
    }

    fn fail_from(&self, error: &str, source: &Value) {
        let mut progress = self
            .progress
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let failure = json!({"stamp_ns":self.clock.now().0,"error":error,"source":source});
        if progress.first_failure.is_none() {
            progress.first_failure = Some(failure.clone());
        }
        progress.failures.push(failure);
        drop(progress);
        self.stop();
    }

    pub(super) fn stop(&self) {
        let _ = self.stop_ns.compare_exchange(
            0,
            self.clock.now().0.max(1),
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        self.wake.notify_one();
    }

    pub(super) fn successful(&self) -> u64 {
        self.successful.load(Ordering::Acquire)
    }

    fn check_drain_deadline(&self, observed_ns: u64) {
        let stop = self.stop_ns.load(Ordering::Acquire);
        if stop != 0
            && stop
                .checked_add(RR_DRAIN_NS)
                .is_none_or(|deadline| observed_ns > deadline)
        {
            self.fail(format!(
                "背景 drain 超过固定 2500ms：stop_ns={stop} observed_ns={observed_ns}"
            ));
        }
    }

    fn update(&self, index: usize, update: impl FnOnce(&mut Value)) {
        let mut progress = self
            .progress
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        update(&mut progress.requests[index]);
    }
}

/// 真实运行与确定性测试共用同一发起/回收接缝；测试只替换时钟与响应 future。
struct PendingTask {
    index: usize,
    started_ns: Arc<AtomicU64>,
}

struct RequestPump {
    schedule: BackgroundSchedule,
    tasks: JoinSet<usize>,
    pending: HashMap<Id, PendingTask>,
    health: BackgroundHealth,
}

impl RequestPump {
    fn new(first_due_ns: u64, health: BackgroundHealth) -> Self {
        Self {
            schedule: BackgroundSchedule::new(first_due_ns),
            tasks: JoinSet::new(),
            pending: HashMap::new(),
            health,
        }
    }

    fn launch_due<F, Fut>(&mut self, request: F) -> Result<bool, String>
    where
        F: FnOnce(CommandMeta) -> Fut + Send + 'static,
        Fut: Future<Output = Result<Ack, tonic::Status>> + Send + 'static,
    {
        self.launch_due_with_preflight(request, |_| {})
    }

    fn launch_due_with_preflight<F, Fut, P>(
        &mut self,
        request: F,
        preflight: P,
    ) -> Result<bool, String>
    where
        F: FnOnce(CommandMeta) -> Fut + Send + 'static,
        Fut: Future<Output = Result<Ack, tonic::Status>> + Send + 'static,
        P: FnOnce(&BackgroundHealth) + Send + 'static,
    {
        self.health.check()?;
        // 已发请求由 JoinSet 独立持有；下一绝对 due 不等待前一回复。
        let admission_ns = self.health.clock.now().0;
        let due = match self.schedule.admit(admission_ns, self.tasks.len()) {
            Ok(Some(due)) => due,
            Ok(None) => return Ok(false),
            Err(error) => {
                let mut progress = self
                    .health
                    .progress
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                progress.admission_rejection = Some(json!({"sequence":progress.requests.len(),
                    "due_ns":self.schedule.next_due_ns,"observed_ns":admission_ns,"tracked":self.tasks.len(),
                    "late":admission_ns.checked_sub(self.schedule.next_due_ns).is_some_and(|lag|lag>=RR_PERIOD_NS),
                    "reason":error,"sent":false}));
                drop(progress);
                self.health.fail(error.clone());
                return Err(error);
            }
        };
        let meta = command_meta(true, 2000);
        let request_id = meta.request_id.clone();
        let request_key = meta.idempotency_key.clone();
        let started_slot = Arc::new(AtomicU64::new(0));
        let index = {
            let mut progress = self
                .health
                .progress
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let index = progress.requests.len();
            progress.starts.push(started_slot.clone());
            progress.requests.push(json!({"sequence":index,"request_id":request_id,"idempotency_key":meta.idempotency_key,
                "due_ns":due,"started_ns":null,"finished_ns":null,"ok":false,"terminal":false}));
            progress.peak_tracked = progress.peak_tracked.max(self.tasks.len() + 1);
            index
        };
        let health = self.health.clone();
        let task_started = started_slot.clone();
        let task = self.tasks.spawn(async move {
            preflight(&health);
            let checked = health.check();
            // 可能等待的健康页锁已经释放；到请求首 poll 之间不再获取证据锁。
            let started = health.clock.now().0;
            task_started.store(started,Ordering::Release);
            let ready = checked.and_then(|()| {
                if started.checked_sub(due).is_none_or(|lag| lag >= RR_PERIOD_NS) {
                    Err("任务实际开始时已经错过 50ms 槽，未发送 RPC".to_owned())
                } else { Ok(()) }
            });
            let mut outcome = match ready {
                Ok(()) => {
                    let deadline = started.checked_add(RR_TIMEOUT_NS);
                    match deadline.and_then(|end| end.checked_sub(health.clock.now().0)) {
                        Some(remaining) if remaining != 0 => {
                            match tokio::time::timeout(Duration::from_nanos(remaining), request(meta)).await {
                                Ok(Ok(ack)) => {
                                    let valid = ack.request_id == request_id && ack.dry_run && ack.cleared_at_ns != 0;
                                    json!({"ok":valid,"kind":"ack","request_id":ack.request_id,
                                        "dry_run":ack.dry_run,"cleared_at_ns":ack.cleared_at_ns})
                                }
                                Ok(Err(status)) => json!({"ok":false,"kind":"status","code":format!("{:?}",status.code()),"message":status.message()}),
                                Err(_) => json!({"ok":false,"kind":"timeout","budget_ns":RR_TIMEOUT_NS}),
                            }
                        }
                        _ => json!({"ok":false,"kind":"deadline_overflow_or_elapsed"}),
                    }
                }
                Err(error) => json!({"ok":false,"kind":"not_sent","error":error}),
            };
            let finished = health.clock.now().0;
            if started.checked_add(RR_TIMEOUT_NS).is_none_or(|deadline|finished>deadline) {
                outcome["response_within_deadline"] = json!(false);
                outcome["ok"] = json!(false);
            }
            if outcome["ok"] == true { health.successful.fetch_add(1, Ordering::AcqRel); }
            else { health.fail_from(&format!("请求 {index} / {request_id}：{outcome}"),
                &json!({"kind":"request","sequence":index,"request_id":request_id,"idempotency_key":request_key})); }
            health.update(index, |row| {
                row["started_ns"] = json!(started);
                row["deadline_ns"] = json!(started.checked_add(RR_TIMEOUT_NS));
                row["finished_ns"] = json!(finished);
                row["ok"] = outcome["ok"].clone(); row["response"] = outcome; row["terminal"] = json!(true);
            });
            index
        });
        self.pending.insert(
            task.id(),
            PendingTask {
                index,
                started_ns: started_slot,
            },
        );
        Ok(true)
    }

    fn completed(&mut self, result: Result<(Id, usize), JoinError>) {
        self.health.check_drain_deadline(self.health.clock.now().0);
        match result {
            Ok((id, _)) => {
                self.pending.remove(&id);
            }
            Err(error) => {
                self.health.fail(format!("背景任务 join 失败：{error}"));
                if let Some(task) = self.pending.remove(&error.id()) {
                    self.health.update(task.index, |row| {
                        let started = task.started_ns.load(Ordering::Acquire);
                        row["started_ns"] = if started == 0 {
                            Value::Null
                        } else {
                            json!(started)
                        };
                        row["deadline_ns"] = if started == 0 {
                            Value::Null
                        } else {
                            json!(started.checked_add(RR_TIMEOUT_NS))
                        };
                        row["terminal"] = json!(true);
                        row["finished_ns"] = json!(self.health.clock.now().0);
                        row["response"] =
                            json!({"ok":false,"kind":"join_error","error":error.to_string()});
                    });
                }
            }
        }
    }

    fn harvest(&mut self) {
        self.health.check_drain_deadline(self.health.clock.now().0);
        while let Some(result) = self.tasks.try_join_next_with_id() {
            self.completed(result);
        }
    }

    async fn run(mut self, client: RobotControlClient<Channel>, run_until_ns: Option<u64>) {
        loop {
            self.harvest();
            if run_until_ns.is_some_and(|end| self.health.clock.now().0 >= end) {
                self.health.stop();
            }
            if self.health.stop_ns.load(Ordering::Acquire) != 0 {
                break;
            }
            let mut client = client.clone();
            if self
                .launch_due(move |meta| async move {
                    client
                        .set_enabled(SetEnabledCommand {
                            meta: Some(meta),
                            enabled: false,
                        })
                        .await
                        .map(tonic::Response::into_inner)
                })
                .is_err()
            {
                break;
            }
            let wait = Duration::from_nanos(
                self.schedule
                    .next_due_ns
                    .saturating_sub(self.health.clock.now().0),
            );
            tokio::select! {
                result = self.tasks.join_next_with_id(), if !self.tasks.is_empty() => {
                    if let Some(result) = result { self.completed(result); }
                }
                () = tokio::time::sleep(wait) => {},
                () = self.health.wake.notified() => {},
            }
        }
        let deadline = self
            .health
            .stop_ns
            .load(Ordering::Acquire)
            .checked_add(RR_DRAIN_NS);
        while !self.tasks.is_empty() {
            self.harvest();
            if self.tasks.is_empty() {
                break;
            }
            let remaining = deadline
                .and_then(|end| end.checked_sub(self.health.clock.now().0))
                .unwrap_or(0);
            if let Ok(Some(result)) = tokio::time::timeout(
                Duration::from_nanos(remaining),
                self.tasks.join_next_with_id(),
            )
            .await
            {
                self.completed(result);
            } else {
                self.health
                    .fail("背景 drain 超过固定 2500ms，仍有未完成 dry-run");
                break;
            }
        }
        let drained = self.health.clock.now().0;
        self.health.check_drain_deadline(drained);
        self.health.drained_ns.store(drained, Ordering::Release);
        // 仅此运行器持有背景干跑，drop 只回收它们，绝不持有 live 恢复 RPC。
    }
}

pub(super) struct Background {
    health: BackgroundHealth,
    started_ns: u64,
    rr: tokio::task::JoinHandle<()>,
    scheduler: std::thread::JoinHandle<Vec<u64>>,
}

impl Background {
    pub(super) fn start(
        client: RobotControlClient<Channel>,
        maximum_ns: Option<u64>,
    ) -> Result<Self, String> {
        let health = BackgroundHealth::new(Arc::new(SystemClock));
        let started_ns = health.clock.now().0;
        let run_until_ns = maximum_ns
            .map(|duration| started_ns.checked_add(duration).ok_or("负载窗口溢出"))
            .transpose()?;
        health
            .run_until_ns
            .store(run_until_ns.unwrap_or(0), Ordering::Release);
        let probe = health.clone();
        let scheduler = std::thread::Builder::new()
            .name("H2调度延迟探针".to_owned())
            .spawn(move || {
                let mut samples = Vec::new();
                let mut due = started_ns;
                while probe.stop_ns.load(Ordering::Acquire) == 0 {
                    let Some(next) = due.checked_add(5_000_000) else {
                        probe.fail("调度探针 due 溢出");
                        break;
                    };
                    due = next;
                    std::thread::sleep(Duration::from_nanos(
                        due.saturating_sub(probe.clock.now().0),
                    ));
                    samples.push(probe.clock.now().0.saturating_sub(due));
                }
                samples
            })
            .map_err(|error| error.to_string())?;
        let rr =
            tokio::spawn(RequestPump::new(started_ns, health.clone()).run(client, run_until_ns));
        Ok(Self {
            health,
            started_ns,
            rr,
            scheduler,
        })
    }

    pub(super) fn health(&self) -> BackgroundHealth {
        self.health.clone()
    }
    pub(super) const fn started_ns(&self) -> u64 {
        self.started_ns
    }

    /// 总是返回完整报告；调用方先落盘，再传播其中失败。
    pub(super) async fn finish(mut self) -> Value {
        self.health.stop();
        let stop_ns = self.health.stop_ns.load(Ordering::Acquire);
        let remaining = stop_ns
            .checked_add(RR_DRAIN_NS)
            .and_then(|end| end.checked_sub(self.health.clock.now().0))
            .unwrap_or(0);
        match tokio::time::timeout(Duration::from_nanos(remaining), &mut self.rr).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => self.health.fail(format!("背景监督任务 join 失败：{error}")),
            Err(_) => {
                self.health.fail("背景监督收尾超过固定 2500ms");
                self.rr.abort();
                let _ = self.rr.await;
            }
        }
        let drained = self.health.drained_ns.load(Ordering::Acquire);
        if drained == 0 {
            self.health.fail("背景监督没有留下实际 drain 完成时刻");
        } else {
            self.health.check_drain_deadline(drained);
        }
        let scheduler = if let Ok(samples) = self.scheduler.join() {
            samples
        } else {
            self.health.fail("调度探针 panic");
            Vec::new()
        };
        background_report(&self.health, self.started_ns, &scheduler)
    }
}

fn background_report(health: &BackgroundHealth, started_ns: u64, scheduler: &[u64]) -> Value {
    let stop_ns = health.stop_ns.load(Ordering::Acquire);
    let progress = health
        .progress
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut rr = progress.requests.clone();
    for (row, started) in rr.iter_mut().zip(&progress.starts) {
        let started = started.load(Ordering::Acquire);
        if row["started_ns"].is_null() && started != 0 {
            row["started_ns"] = json!(started);
            row["deadline_ns"] = json!(started.checked_add(RR_TIMEOUT_NS));
        }
    }
    let starts: Vec<_> = rr
        .iter()
        .filter_map(|row| row["started_ns"].as_u64())
        .collect();
    let gaps: Vec<_> = starts
        .windows(2)
        .map(|pair| pair[1].saturating_sub(pair[0]))
        .collect();
    let unfinished: Vec<_> = rr
        .iter()
        .filter(|row| row["terminal"] != true)
        .cloned()
        .collect();
    let cadence = progress
        .admission_rejection
        .as_ref()
        .is_none_or(|value| value["late"] != true)
        && rr.iter().all(|row| {
            row["started_ns"]
                .as_u64()
                .zip(row["due_ns"].as_u64())
                .is_some_and(|(start, due)| {
                    start.checked_sub(due).is_some_and(|lag| lag < RR_PERIOD_NS)
                })
        });
    let elapsed = starts
        .last()
        .copied()
        .unwrap_or(0)
        .saturating_sub(starts.first().copied().unwrap_or(0));
    let intervals = u32::try_from(starts.len().saturating_sub(1)).unwrap_or(u32::MAX);
    json!({"valid_responses":rr.len()>=2 && progress.first_failure.is_none() && unfinished.is_empty() && rr.iter().all(|row| row["ok"]==true),
            "valid_cadence":cadence,"first_failure":progress.first_failure,"failures":progress.failures,"admission_rejection":progress.admission_rejection,"rr_target_per_second":20,
            "rr_actual_per_second":if elapsed==0 {0.0} else {f64::from(intervals)/Duration::from_nanos(elapsed).as_secs_f64()},
            "started_ns":started_ns,"stop_ns":stop_ns,"drained_ns":health.drained_ns.load(Ordering::Acquire),"finished_ns":health.clock.now().0,
            "launched":rr.len(),"completed":rr.len()-unfinished.len(),"peak_tracked":progress.peak_tracked,
            "tracked_limit":RR_IN_FLIGHT_LIMIT,"client_timeout_ns":RR_TIMEOUT_NS,"drain_budget_ns":RR_DRAIN_NS,
            "unfinished":unfinished,"successful_unique_writes":health.successful(),"rr_requests":rr,
            "rr_gap_ns":super::sla::distribution(&gaps),"sigma_probe_start_ns":started_ns,"sigma_probe_period_ns":5_000_000,
            "sigma_est_ns":super::sla::distribution(scheduler),"sigma_raw_ns":scheduler})
}

#[cfg(test)]
mod tests {
    use super::*;
    use robot_core::time::{MockClock, MonoRawNs};

    #[tokio::test]
    async fn absolute_budget_rejects_drain_harvest_after_fixed_stop_deadline() {
        let clock = Arc::new(MockClock::new(MonoRawNs(1)));
        let mut pump = RequestPump::new(1, BackgroundHealth::new(clock.clone()));
        pump.launch_due(|meta| async move {
            Ok(Ack {
                request_id: meta.request_id,
                dry_run: true,
                cleared_at_ns: 1,
            })
        })
        .unwrap();
        tokio::task::yield_now().await;
        pump.health.stop();
        clock.set(MonoRawNs(1 + RR_DRAIN_NS + 1));
        pump.harvest();
        let report = background_report(&pump.health, 1, &[]);
        assert_eq!(
            report["rr_requests"][0]["ok"], true,
            "及时的原始 Ack 仍保留"
        );
        assert_eq!(
            report["valid_responses"], false,
            "及时 Ack 不能掩盖工具自身回收超时"
        );
        assert_eq!(report["stop_ns"], 1);
        assert!(
            report["first_failure"]["error"]
                .as_str()
                .unwrap()
                .contains("2500ms")
        );
    }

    #[tokio::test]
    async fn absolute_budget_rejects_ack_returned_after_clock_deadline() {
        let clock = Arc::new(MockClock::new(MonoRawNs(1)));
        let mut pump = RequestPump::new(1, BackgroundHealth::new(clock.clone()));
        pump.launch_due(move |meta| async move {
            clock.set(MonoRawNs(1 + RR_TIMEOUT_NS + 1));
            Ok(Ack {
                request_id: meta.request_id,
                dry_run: true,
                cleared_at_ns: 1,
            })
        })
        .unwrap();
        let result = pump.tasks.join_next_with_id().await.unwrap();
        pump.completed(result);
        let report = background_report(&pump.health, 1, &[]);
        assert_eq!(report["successful_unique_writes"], 0);
        assert_eq!(
            report["rr_requests"][0]["response"]["kind"], "ack",
            "原始 Ack 不得丢失"
        );
        assert_eq!(report["rr_requests"][0]["ok"], false);
    }

    #[tokio::test]
    async fn absolute_budget_reads_actual_start_after_evidence_lock() {
        let clock = Arc::new(MockClock::new(MonoRawNs(1)));
        let mut pump = RequestPump::new(1, BackgroundHealth::new(clock.clone()));
        let sent = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let called = sent.clone();
        pump.launch_due_with_preflight(
            move |meta| async move {
                called.store(true, Ordering::Release);
                Ok(Ack {
                    request_id: meta.request_id,
                    dry_run: true,
                    cleared_at_ns: 1,
                })
            },
            move |health| {
                let _guard = health.progress.lock().unwrap();
                // 确定性推进锁持有时间；不得使用锁前时间作为实际发起时间。
                clock.set(MonoRawNs(1 + RR_PERIOD_NS));
            },
        )
        .unwrap();
        let result = pump.tasks.join_next_with_id().await.unwrap();
        pump.completed(result);
        assert!(!sent.load(Ordering::Acquire));
        let report = background_report(&pump.health, 1, &[]);
        assert_eq!(report["rr_requests"][0]["started_ns"], 1 + RR_PERIOD_NS);
        assert_eq!(report["rr_requests"][0]["response"]["kind"], "not_sent");
    }

    #[tokio::test]
    async fn rejected_admission_keeps_exact_due_time_and_cadence_reason() {
        let clock = Arc::new(MockClock::new(MonoRawNs(1)));
        let mut pump = RequestPump::new(1, BackgroundHealth::new(clock.clone()));
        pump.launch_due(|meta| async move {
            Ok(Ack {
                request_id: meta.request_id,
                dry_run: true,
                cleared_at_ns: 1,
            })
        })
        .unwrap();
        let result = pump.tasks.join_next_with_id().await.unwrap();
        pump.completed(result);
        clock.set(MonoRawNs(1 + 2 * RR_PERIOD_NS));
        assert!(pump.launch_due(|_| async { Ok(Ack::default()) }).is_err());
        let report = background_report(&pump.health, 1, &[]);
        assert_eq!(report["rr_requests"][0]["ok"], true);
        assert_eq!(report["valid_cadence"], false);
        assert_eq!(report["admission_rejection"]["due_ns"], 1 + RR_PERIOD_NS);
        assert_eq!(
            report["admission_rejection"]["observed_ns"],
            1 + 2 * RR_PERIOD_NS
        );
        assert_eq!(report["admission_rejection"]["tracked"], 0);
        let mut full = RequestPump::new(
            1,
            BackgroundHealth::new(Arc::new(MockClock::new(MonoRawNs(1)))),
        );
        for _ in 0..RR_IN_FLIGHT_LIMIT {
            full.tasks.spawn(std::future::pending::<usize>());
        }
        assert!(full.launch_due(|_| async { Ok(Ack::default()) }).is_err());
        let report = background_report(&full.health, 1, &[]);
        assert_eq!(report["valid_cadence"], true, "满槽不是迟发，不能混淆原因");
        assert_eq!(report["valid_responses"], false);
        assert_eq!(report["admission_rejection"]["tracked"], 64);
        assert_eq!(report["admission_rejection"]["late"], false);
    }

    #[tokio::test]
    async fn outstanding_dry_response_times_out_and_is_kept_as_failure() {
        let health = BackgroundHealth::new(Arc::new(SystemClock));
        let mut pump = RequestPump::new(health.clock.now().0, health);
        pump.launch_due(|_| std::future::pending::<Result<Ack, tonic::Status>>())
            .unwrap();
        let result = pump.tasks.join_next_with_id().await.unwrap();
        pump.completed(result);
        let report = background_report(&pump.health, 1, &[]);
        assert_eq!(report["rr_requests"][0]["response"]["kind"], "timeout");
        assert_eq!(report["completed"], 1);
        assert_eq!(report["valid_responses"], false);
        assert!(report["first_failure"].is_object());
    }

    #[test]
    fn schedule_boundaries_capacity_overflow_and_first_failure_are_strict() {
        let mut schedule = BackgroundSchedule::new(100);
        assert_eq!(schedule.admit(99, 0), Ok(None));
        assert_eq!(schedule.admit(49_999_999 + 100, 0), Ok(Some(100)));
        let error = schedule.admit(100 + 2 * RR_PERIOD_NS, 0).unwrap_err();
        assert_eq!(schedule.admit(100 + RR_PERIOD_NS, 0), Err(error));
        assert!(BackgroundSchedule::new(1).admit(1, 64).is_err());
        assert_eq!(BackgroundSchedule::new(1).admit(1, 63), Ok(Some(1)));
        assert!(
            BackgroundSchedule::new(u64::MAX - 1)
                .admit(u64::MAX - 1, 0)
                .is_err()
        );
        assert!(
            BackgroundSchedule::new(1)
                .admit(1 + RR_PERIOD_NS, 0)
                .is_err()
        );
    }

    #[tokio::test]
    async fn delayed_task_start_refuses_send_and_keeps_evidence() {
        let clock = Arc::new(MockClock::new(MonoRawNs(1)));
        let mut pump = RequestPump::new(1, BackgroundHealth::new(clock.clone()));
        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let sent = called.clone();
        assert!(
            pump.launch_due(move |_| async move {
                sent.store(true, Ordering::Release);
                Ok(Ack::default())
            })
            .unwrap()
        );
        clock.set(MonoRawNs(1 + RR_PERIOD_NS));
        let result = pump.tasks.join_next_with_id().await.unwrap();
        pump.completed(result);
        assert!(!called.load(Ordering::Acquire));
        assert!(pump.health.check().is_err());
        let progress = pump.health.progress.lock().unwrap();
        assert_eq!(progress.requests[0]["response"]["kind"], "not_sent");
        assert_eq!(progress.requests[0]["terminal"], true);
    }

    #[tokio::test]
    async fn incorrect_ack_and_panicking_response_both_latch_failure() {
        for panic_response in [false, true] {
            let health = BackgroundHealth::new(Arc::new(MockClock::new(MonoRawNs(1))));
            let mut pump = RequestPump::new(1, health);
            pump.launch_due(move |_| async move {
                assert!(!panic_response, "仅测试受控响应 panic");
                Ok(Ack {
                    request_id: "错误身份".to_owned(),
                    dry_run: true,
                    cleared_at_ns: 1,
                })
            })
            .unwrap();
            let result = pump.tasks.join_next_with_id().await.unwrap();
            pump.completed(result);
            assert!(pump.health.check().is_err());
            assert_eq!(pump.health.successful(), 0);
            let progress = pump.health.progress.lock().unwrap();
            assert_eq!(progress.requests[0]["terminal"], true);
            assert_eq!(progress.requests[0]["ok"], false);
        }
    }

    #[tokio::test]
    async fn first_background_failure_waits_for_live_clear_and_never_polls_enable() {
        let health = BackgroundHealth::new(Arc::new(SystemClock));
        let clear_meta = command_meta(false, 2000);
        let clear_id = clear_meta.request_id.clone();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let enabled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let enable_probe = enabled.clone();
        let task_health = health.clone();
        let task = tokio::spawn(async move {
            let mut writes = Vec::new();
            let result = super::super::sla::recover_writes(
                Some(&task_health),
                &mut writes,
                (clear_meta, async move {
                    receiver.await.expect("live Clear 必须等实际结果")
                }),
                (command_meta(false, 2000), async move {
                    enable_probe.store(true, Ordering::Release);
                    Ok(Ack::default())
                }),
            )
            .await;
            (result, writes)
        });
        tokio::task::yield_now().await;
        health.fail("受控背景错误");
        tokio::task::yield_now().await;
        assert!(!task.is_finished(), "不能取消已发 live Clear 逃避退休");
        sender
            .send(Ok(Ack {
                request_id: clear_id,
                dry_run: false,
                cleared_at_ns: 1,
            }))
            .unwrap();
        let (result, writes) = task.await.unwrap();
        assert!(result.is_err());
        assert!(!enabled.load(Ordering::Acquire));
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0]["ok"], true);
    }

    #[tokio::test]
    async fn final_state_three_does_not_hide_disable_capacity_failure() {
        let evidence = super::super::sla::confirm_and_disable(
            async {
                Ok(robot_rpc::proto::RobotState {
                    safety_state: 3,
                    stamp_ns: 19,
                    ..Default::default()
                })
            },
            async {
                Err(tonic::Status::resource_exhausted(
                    "idempotency cache full (4096/4096)",
                ))
            },
            "disable-1",
        )
        .await;
        assert_eq!(evidence["get_state"]["ok"], true);
        assert_eq!(evidence["disable"]["attempted"], true);
        assert_eq!(evidence["disable"]["code"], "ResourceExhausted");
        assert_eq!(evidence["ok"], false);
        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let probe = called.clone();
        let evidence = super::super::sla::confirm_and_disable(
            async { Ok(robot_rpc::proto::RobotState::default()) },
            async move {
                probe.store(true, Ordering::Release);
                Ok(Ack::default())
            },
            "disable-2",
        )
        .await;
        assert!(!called.load(Ordering::Acquire));
        assert_eq!(evidence["disable"]["attempted"], false);
    }

    #[tokio::test]
    async fn final_state_three_does_not_hide_disable_transport_timeout() {
        for status in [
            tonic::Status::cancelled("Timeout expired"),
            tonic::Status::deadline_exceeded("gateway RR timeout"),
        ] {
            let expected_code = format!("{:?}", status.code());
            let evidence = super::super::sla::confirm_and_disable(
                async {
                    Ok(robot_rpc::proto::RobotState {
                        safety_state: 3,
                        ..Default::default()
                    })
                },
                async move { Err(status) },
                "final-disable",
            )
            .await;
            assert_eq!(evidence["get_state"]["ok"], true);
            assert_eq!(evidence["disable"]["attempted"], true);
            assert_eq!(evidence["disable"]["code"], expected_code);
            assert_eq!(evidence["ok"], false);
        }
    }

    #[tokio::test]
    async fn final_disable_keeps_wrong_ack_identity_and_dry_run_for_review() {
        let evidence = super::super::sla::confirm_and_disable(
            async {
                Ok(robot_rpc::proto::RobotState {
                    safety_state: 3,
                    ..Default::default()
                })
            },
            async {
                Ok(Ack {
                    request_id: "wrong-ack".into(),
                    dry_run: true,
                    cleared_at_ns: 7,
                })
            },
            "sent-id",
        )
        .await;
        assert_eq!(evidence["ok"], false);
        assert_eq!(evidence["disable"]["request_id"], "wrong-ack");
        assert_eq!(evidence["disable"]["dry_run"], true);
    }

    #[test]
    fn fixed_load_deadline_stops_new_recovery_without_refresh() {
        let clock = Arc::new(MockClock::new(MonoRawNs(1)));
        let health = BackgroundHealth::new(clock.clone());
        health.run_until_ns.store(10, Ordering::Release);
        health.check().unwrap();
        clock.set(MonoRawNs(10));
        assert!(health.check().is_err());
        clock.set(MonoRawNs(20));
        assert!(health.check().is_err());
        assert_eq!(health.stop_ns.load(Ordering::Acquire), 10);
    }

    #[tokio::test]
    async fn slow_response_does_not_block_next_fixed_due() {
        let clock = Arc::new(MockClock::new(MonoRawNs(1)));
        let health = BackgroundHealth::new(clock.clone());
        let mut pump = RequestPump::new(1, health);
        let mut replies = Vec::new();
        for index in 0..6 {
            clock.set(MonoRawNs(1 + index * RR_PERIOD_NS));
            let (sender, receiver) = tokio::sync::oneshot::channel();
            replies.push(sender);
            assert!(
                pump.launch_due(move |meta| async move {
                    receiver.await.expect("受控回复");
                    Ok(Ack {
                        request_id: meta.request_id,
                        dry_run: true,
                        cleared_at_ns: 1,
                    })
                })
                .unwrap(),
                "前一项未完成也必须发起绝对 due 第 {index} 项"
            );
            tokio::task::yield_now().await;
        }
        clock.set(MonoRawNs(275_000_001));
        for reply in replies.into_iter().rev() {
            reply.send(()).unwrap();
            tokio::task::yield_now().await;
        }
        while let Some(result) = pump.tasks.join_next_with_id().await {
            pump.completed(result);
        }
        assert_eq!(pump.health.successful(), 6);
        pump.health.check().unwrap();
    }
}
