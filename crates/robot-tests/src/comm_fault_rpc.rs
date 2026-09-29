//! 通信故障验收工具共用的 RPC 客户端与最终 Disable 确认流程。
//!
//! Linux 现场工具与跨平台回环测试共用连接配置及回执校验。此模块不直接
//! 访问串口或启动 S0；连接调用方提供的地址，状态确认后发送非 dry-run
//! Disable。回环测试仅连接模拟服务。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use robot_core::time::{RobotClock, SystemClock};
use robot_rpc::proto::robot_control_client::RobotControlClient;
use robot_rpc::proto::{AuthContext, CommandMeta, GetStateRequest, SetEnabledCommand};
use serde_json::{Value, json};
use tonic::transport::{Channel, Endpoint};

static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// 最终 Disable 的业务期限保持 2000ms。
pub const FINAL_DISABLE_TIMEOUT_MS: u64 = 2000;
/// 业务期限加既有 gateway RR grace 500ms 小于此传输期限。
/// GetState 与 Disable 共用此连接，避免客户端提前取消原责任。
pub const FINAL_STOP_TRANSPORT_TIMEOUT: Duration = Duration::from_secs(3);

/// 生成全进程共享序号的验收请求，避免不同调用路径撞幂等键。
#[must_use]
pub fn command_meta(dry_run: bool, timeout_ms: u64) -> CommandMeta {
    let sequence = REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    // 测试 ID 保留 UUIDv7 version/variant，唯一性由进程号和单调序号保证。
    let request_id = format!("{:08x}-0000-7000-8000-{sequence:012x}", std::process::id());
    CommandMeta {
        idempotency_key: request_id.clone(),
        request_id,
        source: "comm_fault_e2e".into(),
        auth: Some(AuthContext {
            token: "operator-fake-token-001".into(),
            mtls_fingerprint: String::new(),
        }),
        timeout_ms,
        dry_run,
        audit_reason: "独立通信故障验收".into(),
    }
}

/// 普通 RPC 保持既有 2 秒传输期限。
pub const ORDINARY_RPC_TRANSPORT_TIMEOUT: Duration = Duration::from_secs(2);

/// 创建普通验收连接。
///
/// # 错误
/// 地址无效或连接重试耗尽时返回错误。
pub async fn connect(address: &str) -> Result<RobotControlClient<Channel>, String> {
    connect_with_request_timeout(address, ORDINARY_RPC_TRANSPORT_TIMEOUT).await
}

async fn connect_with_request_timeout(
    address: &str,
    request_timeout: Duration,
) -> Result<RobotControlClient<Channel>, String> {
    let endpoint = endpoint_with_request_timeout(address, request_timeout)?;
    let deadline = SystemClock.now().0 + 4_000_000_000;
    loop {
        match endpoint.connect().await {
            Ok(channel) => return Ok(RobotControlClient::new(channel)),
            Err(error) if SystemClock.now().0 >= deadline => {
                return Err(format!("连接gateway超时：{error}"));
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
        }
    }
}

fn endpoint_with_request_timeout(
    address: &str,
    request_timeout: Duration,
) -> Result<Endpoint, String> {
    let endpoint = Endpoint::from_shared(format!("http://{address}"))
        .map_err(|error| error.to_string())?
        .connect_timeout(Duration::from_millis(200))
        .timeout(request_timeout);
    Ok(endpoint)
}

/// 通过最终收尾专用连接按顺序确认状态并发送非 dry-run Disable。
/// 连接、状态或回执失败均保留在返回证据中，不以状态 3 代替成功 Ack。
pub async fn final_stop_rpc(address: &str, hardware_id: &str) -> Value {
    match connect_with_request_timeout(address, FINAL_STOP_TRANSPORT_TIMEOUT).await {
        Ok(mut client) => {
            let meta = command_meta(false, FINAL_DISABLE_TIMEOUT_MS);
            let mut disable_client = client.clone();
            let command = SetEnabledCommand {
                meta: Some(meta.clone()),
                enabled: false,
            };
            let mut observed = confirm_and_disable(
                async move {
                    client
                        .get_state(GetStateRequest {
                            hardware_id: hardware_id.to_owned(),
                        })
                        .await
                        .map(tonic::Response::into_inner)
                },
                async move {
                    disable_client
                        .set_enabled(command)
                        .await
                        .map(tonic::Response::into_inner)
                },
                &meta.request_id,
            )
            .await;
            observed["disable"]["request"] = json!({"request_id":meta.request_id,
                "idempotency_key":meta.idempotency_key,"dry_run":false,"enabled":false,"timeout_ms":meta.timeout_ms});
            observed
        }
        Err(error) => json!({"ok":false,"get_state":{"attempted":false,"ok":false,"error":error},
            "disable":{"attempted":false,"ok":false,"reason":"连接失败"}}),
    }
}

/// 保留并校验 Ack 的请求身份、dry-run 和非零完成时间。
#[must_use]
pub fn ack_evidence(
    response: Result<robot_rpc::proto::Ack, tonic::Status>,
    request_id: &str,
    dry_run: bool,
) -> Value {
    match response {
        Ok(ack) => {
            json!({"ok":ack.request_id == request_id && ack.dry_run == dry_run && ack.cleared_at_ns != 0,
            "kind":"ack","request_id":ack.request_id,"dry_run":ack.dry_run,"cleared_at_ns":ack.cleared_at_ns})
        }
        Err(error) => {
            json!({"ok":false,"kind":"status","code":format!("{:?}",error.code()),"message":error.message()})
        }
    }
}

/// 真实收尾使用同一顺序接缝：状态 3 成功并不代表随后 Disable 成功。
pub async fn confirm_and_disable<G, D>(get_state: G, disable: D, request_id: &str) -> Value
where
    G: std::future::Future<Output = Result<robot_rpc::proto::RobotState, tonic::Status>>,
    D: std::future::Future<Output = Result<robot_rpc::proto::Ack, tonic::Status>>,
{
    let observed = match get_state.await {
        Ok(state) => json!({"attempted":true,"ok":state.safety_state == 3,
            "safety_state":state.safety_state,"state_stamp_ns":state.stamp_ns}),
        Err(error) => {
            json!({"attempted":true,"ok":false,"code":format!("{:?}",error.code()),"error":error.message()})
        }
    };
    let disabled = if observed["ok"] == true {
        let started = SystemClock.now().0;
        let mut value = ack_evidence(disable.await, request_id, false);
        value["attempted"] = json!(true);
        value["started_ns"] = json!(started);
        value["finished_ns"] = json!(SystemClock.now().0);
        value
    } else {
        json!({"attempted":false,"ok":false,"reason":"未取得真实状态3确认"})
    };
    json!({"ok":observed["ok"] == true && disabled["ok"] == true,"get_state":observed,"disable":disabled})
}

#[cfg(test)]
mod tests {
    #[test]
    fn final_stop_transport_budget_outlives_gateway_business_deadline() {
        use super::{
            FINAL_DISABLE_TIMEOUT_MS, FINAL_STOP_TRANSPORT_TIMEOUT, ORDINARY_RPC_TRANSPORT_TIMEOUT,
            command_meta,
        };
        use std::time::Duration;

        let disable = command_meta(false, FINAL_DISABLE_TIMEOUT_MS);
        let business_deadline = Duration::from_millis(disable.timeout_ms);
        let gateway_grace = Duration::from_millis(500);
        assert_eq!(ORDINARY_RPC_TRANSPORT_TIMEOUT, Duration::from_secs(2));
        assert_eq!(FINAL_STOP_TRANSPORT_TIMEOUT, Duration::from_secs(3));
        assert!(business_deadline + gateway_grace < FINAL_STOP_TRANSPORT_TIMEOUT);
    }

    #[test]
    fn endpoint_builder_preserves_connect_budget() {
        for request_timeout in [
            super::ORDINARY_RPC_TRANSPORT_TIMEOUT,
            super::FINAL_STOP_TRANSPORT_TIMEOUT,
        ] {
            let endpoint = super::endpoint_with_request_timeout("127.0.0.1:43983", request_timeout)
                .expect("合法 loopback 端点");
            assert_eq!(
                endpoint.get_connect_timeout(),
                Some(std::time::Duration::from_millis(200))
            );
        }
    }
}
