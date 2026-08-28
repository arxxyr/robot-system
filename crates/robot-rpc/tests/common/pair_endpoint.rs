//! `TonicArm`：把一条臂的 tonic `RobotControlClient` 适配成
//! [`robot_rpc::client::dual_arm_workflow::ArmEndpoint`]（v0.5 候选 E 段(7)）。
//!
//! 协议编排住 lib（`client::dual_arm_workflow`，不依赖 tonic `client`
//! feature）；**transport 细节住这里**——`CommandMeta` / token / deadline /
//! 超时 / schema id 全在本文件，e2e 与真机验收 bin 各自实现一份同款适配即可
//! （D9 "同向消费"）。

use std::sync::atomic::{AtomicU64, Ordering};

use tonic::transport::Channel;

use robot_msg::schema_id::SCHEMA_SCRIPTED_PAYLOAD;
use robot_rpc::client::dual_arm_workflow::{ArmEndpoint, ArmRpcError, Side, TaskSnapshot};
use robot_rpc::proto::robot_control_client::RobotControlClient;
use robot_rpc::proto::{
    ActionKindProto, AuthContext, CancelTaskCommand, CommandMeta, EStopCommand, InvokeTaskCommand,
    TaskHandleRef,
};

use crate::common::client::MAINTAINER_TOKEN;

/// 单次 RPC 的 `CommandMeta.timeout_ms`（gateway 侧 RR 等待上限；留足
/// s1 tick 周期 + RR 往返）。
const RPC_TIMEOUT_MS: u64 = 3_000;

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

    async fn invoke_scripted(&mut self, payload: Vec<u8>) -> Result<String, ArmRpcError> {
        let cmd = InvokeTaskCommand {
            meta: Some(self.meta("invoke")),
            kind: ActionKindProto::ActionKindScripted as i32,
            // 0 = 不设 deadline（chunk 时长由 waypoint 段时长决定）。
            deadline_ns: 0,
            priority: 10,
            payload_schema_id: SCHEMA_SCRIPTED_PAYLOAD,
            payload,
        };
        let handle = self
            .client
            .invoke_task(cmd)
            .await
            .map_err(|s| map_status(&s))?
            .into_inner();
        Ok(handle.action_id)
    }

    async fn cancel_task(&mut self, action_id: &str) -> Result<(), ArmRpcError> {
        let cmd = CancelTaskCommand {
            meta: Some(self.meta("cancel")),
            action_id: action_id.to_string(),
        };
        self.client
            .cancel_task(cmd)
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
            .get_task_status(req)
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
        self.client.e_stop(cmd).await.map_err(|s| map_status(&s))?;
        Ok(())
    }
}
