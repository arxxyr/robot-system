//! gateway 地址解析 + 连接（Q-CLI4(a)）。

use std::time::Duration;

use robot_rpc::proto::robot_control_client::RobotControlClient;
use tonic::transport::{Channel, Endpoint};

/// 默认 gateway 地址（loopback；Q-CLI4(a)）。
pub const DEFAULT_GATEWAY_ADDR: &str = "127.0.0.1:50051";

/// gateway 地址解析：`--gateway-addr` 优先 → env `ROBOT_GATEWAY_ADDR` →
/// 默认 [`DEFAULT_GATEWAY_ADDR`]。纯函数（env 值由调用方读入传参）。
#[must_use]
pub fn resolve_gateway_addr(flag: Option<&str>, env_addr: Option<&str>) -> String {
    flag.filter(|a| !a.is_empty())
        .or_else(|| env_addr.filter(|a| !a.is_empty()))
        .unwrap_or(DEFAULT_GATEWAY_ADDR)
        .to_owned()
}

/// 连接 gateway（loopback；`http://<addr>`）。`connect_timeout` 只控制**建链**
/// 上限；失败返回人类可读消息（CLI 走 exit 5）。
///
/// **单 RPC deadline 不在此设**（(2/5) amend）：endpoint 的 per-request
/// `.timeout` 超时码不稳定（tonic 可能落 `Cancelled` → exit 7，而 AC-5 要
/// client timeout → exit 6）。请求 deadline 改由调用方用 `tokio::time::
/// timeout` 包裹 RPC（见 [`crate::dispatch`]）独占，超时稳定 exit 6。
///
/// (1/5) 作为共享 lib 落地（pub API）；binary 从 (2/5) 第一个 async 子命令
/// 起调用。连接真链路由 `robotctl_smoke` cross-process e2e 覆盖（(5/5)）。
pub async fn connect_gateway(
    addr: &str,
    connect_timeout: Duration,
) -> Result<RobotControlClient<Channel>, String> {
    let url = format!("http://{addr}");
    let endpoint = Endpoint::try_from(url.clone())
        .map_err(|e| format!("invalid gateway endpoint {url}: {e}"))?
        .connect_timeout(connect_timeout);
    let channel = endpoint
        .connect()
        .await
        .map_err(|e| format!("connect gateway {url}: {e}"))?;
    Ok(RobotControlClient::new(channel))
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_GATEWAY_ADDR, resolve_gateway_addr};

    #[test]
    fn addr_prefers_flag_then_env_then_default() {
        assert_eq!(
            resolve_gateway_addr(Some("1.2.3.4:9"), Some("5.6.7.8:9")),
            "1.2.3.4:9"
        );
        assert_eq!(resolve_gateway_addr(None, Some("5.6.7.8:9")), "5.6.7.8:9");
        assert_eq!(resolve_gateway_addr(Some(""), None), DEFAULT_GATEWAY_ADDR);
        assert_eq!(resolve_gateway_addr(None, None), DEFAULT_GATEWAY_ADDR);
        assert_eq!(
            resolve_gateway_addr(Some(""), Some("")),
            DEFAULT_GATEWAY_ADDR
        );
    }
}
