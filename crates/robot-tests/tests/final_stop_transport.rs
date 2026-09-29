//! 最终 Disable 的真实 tonic loopback 传输回归。
//!
//! 显式运行：cargo test -p robot-tests --features comm-fault-rpc --test
//! final_stop_transport -- --ignored --test-threads=1 --nocapture。
//! 服务端只模拟 GetState/Ack；不启动 S0、RR worker、串口或电机，不验证
//! gateway grace、Lower 退休、物理断扭或现场 H2 SLA。

#![allow(clippy::doc_markdown)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use robot_core::time::{RobotClock, SystemClock};
use robot_rpc::proto::robot_control_server::{RobotControl, RobotControlServer};
use robot_rpc::proto::{
    Ack, CancelTaskCommand, ClearEStopCommand, ClearErrorCommand, EStopCommand, GetStateRequest,
    InvokeTaskCommand, RobotState, SetEnabledCommand, StreamRequest, TaskEvent, TaskHandle,
    TaskHandleRef, TaskStatus,
};
use robot_tests::comm_fault_rpc::{
    FINAL_DISABLE_TIMEOUT_MS, command_meta, confirm_and_disable, connect, final_stop_rpc,
};
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tonic::{Code, Request, Response, Status};

const HARDWARE_ID: &str = "loopback_arm";
const DELAYED_ACK: Duration = Duration::from_millis(2100);
const CALL_BOUND: Duration = Duration::from_secs(6);
const SHUTDOWN_BOUND: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug)]
enum Reply {
    Ack,
    WrongIdentity,
    DryRun,
    ZeroTimestamp,
    Error,
}

#[derive(Clone, Copy)]
struct Plan {
    delay: Duration,
    reply: Reply,
    safety_state: i32,
    state_error: bool,
}

impl Plan {
    fn ack(delay: Duration) -> Self {
        Self {
            delay,
            reply: Reply::Ack,
            safety_state: 3,
            state_error: false,
        }
    }
}

#[derive(Clone, Debug)]
enum Received {
    State(GetStateRequest),
    Disable(SetEnabledCommand),
}

#[derive(Default)]
struct Record {
    requests: Mutex<Vec<Received>>,
    active: AtomicUsize,
    reply_elapsed_ns: Mutex<Option<u64>>,
}

struct ActiveRequest(Arc<Record>);

impl Drop for ActiveRequest {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}

struct DelayedService {
    plan: Plan,
    record: Arc<Record>,
}

#[tonic::async_trait]
impl RobotControl for DelayedService {
    async fn get_state(
        &self,
        request: Request<GetStateRequest>,
    ) -> Result<Response<RobotState>, Status> {
        self.record
            .requests
            .lock()
            .expect("请求记录锁")
            .push(Received::State(request.into_inner()));
        if self.plan.state_error {
            return Err(Status::unavailable("模拟状态不可读"));
        }
        Ok(Response::new(RobotState {
            safety_state: self.plan.safety_state,
            stamp_ns: 19,
            ..Default::default()
        }))
    }

    async fn set_enabled(
        &self,
        request: Request<SetEnabledCommand>,
    ) -> Result<Response<Ack>, Status> {
        let command = request.into_inner();
        self.record
            .requests
            .lock()
            .expect("请求记录锁")
            .push(Received::Disable(command.clone()));
        self.record.active.fetch_add(1, Ordering::SeqCst);
        let _active = ActiveRequest(Arc::clone(&self.record));
        let started_ns = SystemClock.now().0;
        // 实际最小延迟由 RobotClock 确认，不把 sleep 的名义时长当观测值。
        loop {
            let elapsed = Duration::from_nanos(SystemClock.now().0.saturating_sub(started_ns));
            match self.plan.delay.checked_sub(elapsed) {
                Some(remaining) if !remaining.is_zero() => tokio::time::sleep(remaining).await,
                _ => break,
            }
        }
        *self.record.reply_elapsed_ns.lock().expect("回执计时锁") =
            Some(SystemClock.now().0.saturating_sub(started_ns));
        let meta = command
            .meta
            .ok_or_else(|| Status::invalid_argument("缺少 CommandMeta"))?;
        let mut ack = Ack {
            request_id: meta.request_id,
            dry_run: meta.dry_run,
            cleared_at_ns: 7,
        };
        match self.plan.reply {
            Reply::Ack => {}
            Reply::WrongIdentity => ack.request_id = "wrong-ack".into(),
            Reply::DryRun => ack.dry_run = true,
            Reply::ZeroTimestamp => ack.cleared_at_ns = 0,
            Reply::Error => return Err(Status::resource_exhausted("模拟责任账本拒绝")),
        }
        Ok(Response::new(ack))
    }

    async fn clear_e_stop(&self, _: Request<ClearEStopCommand>) -> Result<Response<Ack>, Status> {
        Err(Status::unimplemented("本回归不允许清除 EStop"))
    }
    async fn invoke_task(
        &self,
        _: Request<InvokeTaskCommand>,
    ) -> Result<Response<TaskHandle>, Status> {
        Err(Status::unimplemented("本回归不允许任务调用"))
    }
    async fn cancel_task(&self, _: Request<CancelTaskCommand>) -> Result<Response<Ack>, Status> {
        Err(Status::unimplemented("本回归不提供任务取消"))
    }
    async fn get_task_status(
        &self,
        _: Request<TaskHandleRef>,
    ) -> Result<Response<TaskStatus>, Status> {
        Err(Status::unimplemented("本回归不提供任务状态"))
    }
    type StreamTaskEventsStream = tonic::codegen::BoxStream<TaskEvent>;
    async fn stream_task_events(
        &self,
        _: Request<TaskHandleRef>,
    ) -> Result<Response<Self::StreamTaskEventsStream>, Status> {
        Err(Status::unimplemented("本回归不提供任务事件"))
    }
    async fn clear_error(&self, _: Request<ClearErrorCommand>) -> Result<Response<Ack>, Status> {
        Err(Status::unimplemented("本回归不允许清除错误"))
    }
    type StreamStateStream = tonic::codegen::BoxStream<RobotState>;
    async fn stream_state(
        &self,
        _: Request<StreamRequest>,
    ) -> Result<Response<Self::StreamStateStream>, Status> {
        Err(Status::unimplemented("本回归不提供状态流"))
    }
    async fn e_stop(&self, _: Request<EStopCommand>) -> Result<Response<Ack>, Status> {
        Err(Status::unimplemented("本回归不触发 EStop"))
    }
}

struct Observation {
    evidence: Value,
    requests: Vec<Received>,
    reply_elapsed_ns: Option<u64>,
}

async fn ordinary_control(address: &str) -> Result<Value, String> {
    let mut client = connect(address).await?;
    let mut disable_client = client.clone();
    let meta = command_meta(false, FINAL_DISABLE_TIMEOUT_MS);
    let command = SetEnabledCommand {
        meta: Some(meta.clone()),
        enabled: false,
    };
    Ok(confirm_and_disable(
        async move {
            client
                .get_state(GetStateRequest {
                    hardware_id: HARDWARE_ID.into(),
                })
                .await
                .map(Response::into_inner)
        },
        async move {
            disable_client
                .set_enabled(command)
                .await
                .map(Response::into_inner)
        },
        &meta.request_id,
    )
    .await)
}

async fn observe(plan: Plan, ordinary: bool) -> Observation {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("绑定独占回环端口");
    let address = listener.local_addr().expect("读取回环地址").to_string();
    let record = Arc::new(Record::default());
    let service = DelayedService {
        plan,
        record: Arc::clone(&record),
    };
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let mut server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(RobotControlServer::new(service))
            .serve_with_incoming_shutdown(
                tonic::transport::server::TcpIncoming::from(listener),
                async {
                    let _ = shutdown_rx.await;
                },
            )
            .await
    });
    let outcome = tokio::time::timeout(CALL_BOUND, async {
        if ordinary {
            ordinary_control(&address).await
        } else {
            Ok(final_stop_rpc(&address, HARDWARE_ID).await)
        }
    })
    .await;
    // 成功、客户端超时和回执错误都先收尾，再执行验收断言。
    let _ = shutdown_tx.send(());
    let shutdown = tokio::time::timeout(SHUTDOWN_BOUND, &mut server).await;
    if shutdown.is_err() {
        server.abort();
        let reaped = tokio::time::timeout(SHUTDOWN_BOUND, &mut server).await;
        panic!("tonic 服务未按时退出，取消后的真实回收结果：{reaped:?}");
    }
    shutdown
        .expect("已检查退出超时")
        .expect("回收服务任务")
        .expect("服务正常退出");
    assert_eq!(
        record.active.load(Ordering::SeqCst),
        0,
        "服务退出后不得残留处理中的请求"
    );
    let evidence = outcome
        .expect("客户端流程必须有界完成")
        .expect("普通负控连接成功");
    let requests = record.requests.lock().expect("请求记录锁").clone();
    let reply_elapsed_ns = *record.reply_elapsed_ns.lock().expect("回执计时锁");
    println!(
        "{}",
        json!({"ordinary":ordinary,"delay_ms":plan.delay.as_millis(),"reply":format!("{:?}",plan.reply),"evidence":evidence,"server_reply_elapsed_ns":reply_elapsed_ns,"server_joined":true,"active_requests":0})
    );
    Observation {
        evidence,
        requests,
        reply_elapsed_ns,
    }
}

fn verify_requests(observed: &Observation, disable_sent: bool) {
    assert_eq!(observed.requests.len(), if disable_sent { 2 } else { 1 });
    let Received::State(state) = &observed.requests[0] else {
        panic!("首个请求必须为 GetState")
    };
    assert_eq!(state.hardware_id, HARDWARE_ID);
    if !disable_sent {
        return;
    }
    let Received::Disable(command) = &observed.requests[1] else {
        panic!("第二个请求必须为 Disable")
    };
    robot_rpc::set_enabled::validate_set_enabled_command(command)
        .expect("真实请求通过既有协议校验");
    let meta = command.meta.as_ref().expect("真实请求携带 CommandMeta");
    assert!(!command.enabled);
    assert!(!meta.dry_run);
    assert_eq!(meta.timeout_ms, 2000);
    assert_ne!(meta.request_id, "");
    assert_eq!(meta.request_id, meta.idempotency_key);
    if let Some(sent) = observed.evidence["disable"].get("request") {
        assert_eq!(sent["request_id"], meta.request_id);
        assert_eq!(sent["idempotency_key"], meta.idempotency_key);
        assert_eq!(sent["dry_run"], false);
        assert_eq!(sent["enabled"], false);
        assert_eq!(sent["timeout_ms"], 2000);
    }
}

// tonic 的计时域与 RobotClock 不同；只记录 RAW 耗时，不用跨时钟精确边界判超时。
// 固定 2/3 秒预算由共享模块纯测试锁定，此处要求实际网络请求被 Endpoint 取消。
fn verify_timeout(observed: &Observation) {
    verify_requests(observed, true);
    assert_eq!(observed.evidence["get_state"]["ok"], true);
    assert_eq!(observed.evidence["ok"], false);
    assert_eq!(observed.evidence["disable"]["kind"], "status");
    assert_eq!(
        observed.evidence["disable"]["code"],
        format!("{:?}", Code::Cancelled)
    );
    assert_eq!(observed.evidence["disable"]["message"], "Timeout expired");
}

#[tokio::test]
#[ignore = "显式 loopback 集成测试，需要真实 2.1 秒延迟"]
async fn final_disable_accepts_2100ms_ack() {
    let observed = observe(Plan::ack(DELAYED_ACK), false).await;
    verify_requests(&observed, true);
    assert_eq!(observed.evidence["ok"], true);
    assert_eq!(
        observed.evidence["disable"]["request_id"],
        observed.evidence["disable"]["request"]["request_id"]
    );
    assert!(
        u128::from(observed.reply_elapsed_ns.expect("服务实际回执时间")) >= DELAYED_ACK.as_nanos()
    );
}

#[tokio::test]
#[ignore = "显式 loopback 集成测试，需要真实 2 秒传输超时"]
async fn ordinary_endpoint_rejects_same_2100ms_ack() {
    let observed = observe(Plan::ack(DELAYED_ACK), true).await;
    verify_timeout(&observed);
}

#[tokio::test]
#[ignore = "显式 loopback 集成测试，需要真实 3 秒传输超时"]
async fn final_disable_rejects_reply_after_three_seconds() {
    let observed = observe(Plan::ack(Duration::from_millis(3300)), false).await;
    verify_timeout(&observed);
}

#[tokio::test]
#[ignore = "显式 loopback 集成测试"]
async fn final_disable_preserves_server_error_after_state_three() {
    let observed = observe(
        Plan {
            reply: Reply::Error,
            ..Plan::ack(Duration::ZERO)
        },
        false,
    )
    .await;
    verify_requests(&observed, true);
    assert_eq!(observed.evidence["get_state"]["ok"], true);
    assert_eq!(observed.evidence["ok"], false);
    assert_eq!(observed.evidence["disable"]["code"], "ResourceExhausted");
    assert_eq!(observed.evidence["disable"]["message"], "模拟责任账本拒绝");
}

#[tokio::test]
#[ignore = "显式 loopback 集成测试"]
async fn final_disable_rejects_wrong_ack_identity_dry_run_and_zero_timestamp() {
    for reply in [Reply::WrongIdentity, Reply::DryRun, Reply::ZeroTimestamp] {
        let observed = observe(
            Plan {
                reply,
                ..Plan::ack(Duration::ZERO)
            },
            false,
        )
        .await;
        verify_requests(&observed, true);
        assert_eq!(observed.evidence["get_state"]["ok"], true);
        assert_eq!(observed.evidence["ok"], false);
        assert_eq!(observed.evidence["disable"]["kind"], "ack");
        match reply {
            Reply::WrongIdentity => {
                assert_eq!(observed.evidence["disable"]["request_id"], "wrong-ack");
            }
            Reply::DryRun => assert_eq!(observed.evidence["disable"]["dry_run"], true),
            Reply::ZeroTimestamp => assert_eq!(observed.evidence["disable"]["cleared_at_ns"], 0),
            Reply::Ack | Reply::Error => unreachable!("仅遍历三种错误 Ack"),
        }
    }
}

#[tokio::test]
#[ignore = "显式 loopback 集成测试"]
async fn final_disable_requires_state_three_and_successful_state_rpc() {
    for state_error in [false, true] {
        let observed = observe(
            Plan {
                safety_state: 2,
                state_error,
                ..Plan::ack(Duration::ZERO)
            },
            false,
        )
        .await;
        verify_requests(&observed, false);
        assert_eq!(observed.evidence["ok"], false);
        assert_eq!(observed.evidence["get_state"]["ok"], false);
        assert_eq!(observed.evidence["disable"]["attempted"], false);
        if state_error {
            assert_eq!(observed.evidence["get_state"]["code"], "Unavailable");
        }
    }
}
