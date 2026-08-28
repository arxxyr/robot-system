//! `TonicArm`：把一条臂的 tonic `RobotControlClient` 适配成
//! [`robot_rpc::client::dual_arm_workflow::ArmEndpoint`]（v0.5 候选 E 段(7)）。
//!
//! 协议编排住 lib（`client::dual_arm_workflow`，不依赖 tonic `client`
//! feature）；**transport 细节住这里**——`CommandMeta` / token / deadline /
//! schema id 全在本文件，e2e 与真机验收 bin 各自实现一份同款适配即可
//! （D9 "同向消费"）。
//!
//! ## 超时层级（round 1 阻断 1；round 2 修正顺序与数值）
//!
//! 内 → 外**严格递增**，让最内层先给出**语义最丰富**的答复：
//!
//! | 层 | 值 | 谁先响应 | 副作用语义 |
//! |---|---|---|---|
//! | gateway RR 等待（`CommandMeta.timeout_ms`） | 2 s | 返 typed status | 视 code 而定：白名单内（如 `FailedPrecondition`）= 明确拒绝；**`DeadlineExceeded` = 结果不确定**（请求已 forward 给 S1） |
//! | 端点 transport（`Request::set_timeout`） | 3 s | tonic 断连 → `Status(Cancelled)` | **结果不确定** |
//! | 协议兜底（`PairConfig::rpc_timeout`） | 4 s（lib 默认） | 端点漏设超时时的最后一道 | **结果不确定** |
//!
//! 顺序**递增**的收益是：保留拿到**白名单内明确拒绝**与更丰富诊断信息的
//! 机会（最内层先答复）。它**不会**把 `DeadlineExceeded` 之类变成明确拒绝
//! ——分类只看 `DEFINITE_REJECTION_CODES` 白名单；顺序反了则连"可证明未
//! 启动"的拒绝也拿不到，一律退化成不确定 + 双 EStop。
//!
//! ## action seam + 失败分类（round 3 P0-1）
//!
//! 实现 [`ArmEndpoint::invoke_action`]（通用 `ActionSpec`），**不**假定
//! kind——`ActionSpec::scripted` 与 `ActionSpec::dwell_no_payload`
//! （test profile 下路由到 `fault_inject`）在调用点显式区分。
//!
//! invoke 失败按 **gRPC code 契约白名单**（`SubmitFailure::from_status_code`
//! / `DEFINITE_REJECTION_CODES`）分成"明确拒绝"与"结果不确定"：gateway 在
//! forward 给 S1 **之后**仍可能返 `DeadlineExceeded`（RR 等待超时）/
//! `Internal`（response decode / request-id 不匹配）/ `Unavailable`
//! （oneshot 丢失），tonic 的 transport 失败也表现为 `Cancelled` /
//! `Unavailable` / `Unknown`——这些**都不能**证明 S1 未接受 action，一律
//! 归 `Indeterminate`。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tonic::transport::Channel;

use robot_rpc::client::dual_arm_workflow::{
    ActionSpec, ArmEndpoint, ArmRpcError, Side, SubmitFailure, TaskSnapshot,
};
use robot_rpc::proto::robot_control_client::RobotControlClient;
use robot_rpc::proto::{
    AuthContext, CancelTaskCommand, CommandMeta, EStopCommand, InvokeTaskCommand, TaskHandleRef,
};

use crate::common::client::MAINTAINER_TOKEN;

/// 单次 RPC 的 `CommandMeta.timeout_ms`（gateway 侧 RR 等待上限；留足
/// s1 tick 周期 + RR 往返）。**最内层**——先于 transport / 协议超时给出
/// typed status。
const RPC_TIMEOUT_MS: u64 = 2_000;
/// **transport 层**硬超时：> `RPC_TIMEOUT_MS`，让 gateway 有机会先返 typed
/// status；再超才由 tonic 断开。仍 < 协议 `PairConfig::rpc_timeout`（4 s）。
const TRANSPORT_TIMEOUT: Duration = Duration::from_millis(RPC_TIMEOUT_MS + 1_000);

/// 一条臂的 tonic 端点。
pub struct TonicArm {
    side: Side,
    client: RobotControlClient<Channel>,
    token: &'static str,
    /// 每次调用递增，拼进 `idempotency_key` 保证不同请求不撞幂等缓存。
    seq: AtomicU64,
}

impl TonicArm {
    /// 用 [`crate::common::dual_ctx::ArmStack::channel`] 构造。
    #[must_use]
    pub fn new(side: Side, channel: Channel) -> Self {
        Self {
            side,
            client: RobotControlClient::new(channel),
            token: MAINTAINER_TOKEN,
            seq: AtomicU64::new(0),
        }
    }

    /// 直接拿底层 client（case 需要 `GetState` / `ClearError` 等非协议 RPC 时）。
    pub fn client_mut(&mut self) -> &mut RobotControlClient<Channel> {
        &mut self.client
    }

    fn meta(&self, purpose: &str) -> CommandMeta {
        let n = self.seq.fetch_add(1, Ordering::Relaxed);
        CommandMeta {
            request_id: uuid::Uuid::now_v7().to_string(),
            source: format!("dimos_e2e/{}", self.side.label()),
            auth: Some(AuthContext {
                token: self.token.to_string(),
                mtls_fingerprint: String::new(),
            }),
            timeout_ms: RPC_TIMEOUT_MS,
            idempotency_key: format!("{}-{purpose}-{n}", self.side.label()),
            dry_run: false,
            audit_reason: format!("dimos workflow e2e {purpose}"),
        }
    }
}

/// 包 tonic `Request` 并设 **transport 硬超时**（round 1 阻断 1）。
fn with_timeout<T>(msg: T) -> tonic::Request<T> {
    let mut req = tonic::Request::new(msg);
    req.set_timeout(TRANSPORT_TIMEOUT);
    req
}

/// tonic `Status` / transport 错 → 协议侧 [`ArmRpcError`]。
fn map_status(status: &tonic::Status) -> ArmRpcError {
    ArmRpcError::Status {
        code: format!("{:?}", status.code()),
        message: status.message().to_string(),
    }
}

impl ArmEndpoint for TonicArm {
    fn side(&self) -> Side {
        self.side
    }

    async fn invoke_action(&mut self, spec: ActionSpec) -> Result<String, SubmitFailure> {
        let cmd = InvokeTaskCommand {
            meta: Some(self.meta("invoke")),
            kind: spec.kind.proto_value(),
            // 0 = 不设 deadline（chunk 时长由 waypoint 段时长决定）。
            deadline_ns: 0,
            priority: 10,
            payload_schema_id: spec.payload_schema_id,
            payload: spec.payload,
        };
        let handle = self
            .client
            .invoke_task(with_timeout(cmd))
            .await
            // **按 code 分类**（round 3 P0-1）：tonic 的 `Code` Debug 名与
            // `DEFINITE_REJECTION_CODES` 同字面；白名单外一律 Indeterminate。
            .map_err(|s| {
                SubmitFailure::from_status_code(&format!("{:?}", s.code()), map_status(&s))
            })?
            .into_inner();
        Ok(handle.action_id)
    }

    async fn cancel_task(&mut self, action_id: &str) -> Result<(), ArmRpcError> {
        let cmd = CancelTaskCommand {
            meta: Some(self.meta("cancel")),
            action_id: action_id.to_string(),
        };
        self.client
            .cancel_task(with_timeout(cmd))
            .await
            .map_err(|s| map_status(&s))?;
        Ok(())
    }

    async fn task_status(&mut self, action_id: &str) -> Result<TaskSnapshot, ArmRpcError> {
        let req = TaskHandleRef {
            request_id: uuid::Uuid::now_v7().to_string(),
            action_id: action_id.to_string(),
        };
        let status = self
            .client
            .get_task_status(with_timeout(req))
            .await
            .map_err(|s| map_status(&s))?
            .into_inner();
        Ok(TaskSnapshot::from_proto(&status))
    }

    async fn estop(&mut self, reason: &str) -> Result<(), ArmRpcError> {
        let cmd = EStopCommand {
            meta: Some(self.meta("estop")),
            estop_reason: reason.to_string(),
        };
        self.client
            .e_stop(with_timeout(cmd))
            .await
            .map_err(|s| map_status(&s))?;
        Ok(())
    }
}
