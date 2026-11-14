//! 受监督单关节任务的WebSocket使能与提交；观察和失败收尾沿原维护调用链。

use std::net::IpAddr;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderValue, Uri, header, uri::PathAndQuery};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async_with_config};

use super::{ActionSpec, CLIENT_HARD_TIMEOUT, uuidv7_compat};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub(super) struct Config {
    url: String,
    origin: String,
    bus_id: String,
}

impl Config {
    pub(super) fn new(url: &str, bus_id: &str) -> Result<Self, String> {
        let uri: Uri = url
            .parse()
            .map_err(|error| format!("Web地址无效：{error}"))?;
        let host = uri.host().unwrap_or_default().trim_matches(['[', ']']);
        let local = host.parse::<IpAddr>().is_ok_and(|address| match address {
            IpAddr::V4(ip) => ip.is_loopback() || ip.is_private(),
            IpAddr::V6(ip) => ip.is_loopback(),
        });
        if uri.scheme_str() != Some("ws")
            || !local
            || uri.path_and_query().map(PathAndQuery::as_str) != Some("/api/ws")
            || uri
                .authority()
                .is_none_or(|authority| authority.as_str().contains('@'))
            || bus_id.is_empty()
            || bus_id.len() > 64
            || !bus_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
        {
            return Err("Web验收仅接受显式内网/回环ws://IP:PORT/api/ws及有效总线标识".into());
        }
        let authority = uri.authority().ok_or("Web地址缺少主机")?;
        Ok(Self {
            url: url.into(),
            origin: format!("http://{authority}"),
            bus_id: bus_id.into(),
        })
    }
}

pub(super) struct Session {
    socket: Option<Socket>,
    config: Config,
    robot_id: String,
    hardware_id: String,
    layout_id: String,
    enable_attempted: bool,
}

struct Ready {
    process_id: String,
    revision: u64,
}

impl Session {
    pub(super) async fn connect(
        config: Config,
        robot_id: &str,
        hardware_id: &str,
        layout_id: u64,
    ) -> Result<Self, String> {
        let mut request = config
            .url
            .as_str()
            .into_client_request()
            .map_err(|error| error.to_string())?;
        request.headers_mut().insert(
            header::ORIGIN,
            HeaderValue::from_str(&config.origin).map_err(|error| error.to_string())?,
        );
        let limits = WebSocketConfig::default()
            .read_buffer_size(64 * 1024)
            .write_buffer_size(0)
            .max_write_buffer_size(64 * 1024)
            .max_message_size(Some(64 * 1024))
            .max_frame_size(Some(64 * 1024));
        let (socket, _) = tokio::time::timeout(
            CLIENT_HARD_TIMEOUT,
            connect_async_with_config(request, Some(limits), false),
        )
        .await
        .map_err(|_| "Web握手超时，未发送任务")?
        .map_err(|error| format!("Web握手失败，未发送任务：{error}"))?;
        Ok(Self {
            socket: Some(socket),
            config,
            robot_id: robot_id.into(),
            hardware_id: hardware_id.into(),
            layout_id: layout_id.to_string(),
            enable_attempted: false,
        })
    }

    /// 本次使能仅发一次；未知结果不重发、不回退直接RPC，调用方执行原收尾。
    pub(super) async fn enable(&mut self, minimum_stamp_ns: u64) -> Result<(), String> {
        if self.enable_attempted {
            return Err("本次Web使能已尝试，不得重发".into());
        }
        self.enable_attempted = true;
        // 先移出连接；future取消或失败会关闭它，不能继续提交运动。
        let mut socket = self.socket.take().ok_or("Web使能连接缺失")?;
        let ready = loop {
            let value = receive_json(&mut socket).await?;
            if let Some(ready) = self.ready_for_enable(&value, minimum_stamp_ns)? {
                break ready;
            }
            tokio::task::yield_now().await;
        };
        let expected_revision = ready
            .revision
            .checked_add(1)
            .ok_or("Web版本耗尽，未发送使能")?
            .to_string();
        let request_id = uuidv7_compat();
        let request = json!({
            "type": "control", "operation": "enable", "request_id": request_id,
            "bus_id": self.config.bus_id, "process_id": ready.process_id,
            "revision": ready.revision.to_string(),
        });
        println!(
            "WEB_ENABLE_REQUEST request_id={request_id} bus_id={}",
            self.config.bus_id
        );
        socket
            .send(Message::Text(request.to_string().into()))
            .await
            .map_err(|error| format!("Web使能发送结果未知，不重试：{error}"))?;
        loop {
            let value = receive_json(&mut socket).await?;
            if value["type"] == "control_result" {
                if value["request_id"] != request_id
                    || value["bus_id"] != self.config.bus_id
                    || value["operation"] != "enable"
                    || value["status"] != "accepted"
                    || value["revision"].as_str() != Some(expected_revision.as_str())
                    || !value["action_id"].is_null()
                {
                    return Err(format!("Web使能未确认，不重试：{}", value["message"]));
                }
                println!("WEB_ENABLE_ACCEPTED request_id={request_id}");
                self.socket = Some(socket);
                return Ok(());
            }
            if self.bus(&value)?["control"]["process_id"] != ready.process_id {
                return Err("Web进程变化，使能结果未知，不重试".into());
            }
            tokio::task::yield_now().await;
        }
    }

    fn ready_for_enable(
        &self,
        value: &Value,
        minimum_stamp_ns: u64,
    ) -> Result<Option<Ready>, String> {
        let bus = self.bus(value)?;
        let control = &bus["control"];
        if control["enabled"] != true || control["busy"] != false {
            return Err("Web控制未开放或忙碌，未发送使能".into());
        }
        let stamp = bus["stamp_ns"].as_str().and_then(|v| v.parse::<u64>().ok());
        if bus["data_status"] != "fresh"
            || bus["mode"] != "Safe"
            || bus["safety_state"] != "TorqueOffConfirmed"
            || bus["fault_code"] != 0
            || stamp.is_none_or(|v| v < minimum_stamp_ns)
            || control["can_enable"] != true
        {
            return Ok(None);
        }
        let process_id = control["process_id"].as_str().ok_or("Web进程身份缺失")?;
        robot_rpc::auth::validate_request_id(process_id).map_err(|e| e.to_string())?;
        let revision = control["revision"]
            .as_str()
            .and_then(|v| v.parse().ok())
            .ok_or("Web版本无效")?;
        Ok(Some(Ready {
            process_id: process_id.into(),
            revision,
        }))
    }

    /// 调用方原4秒提交期限覆盖等待快照、发送与回执；取消future会释放唯一连接。
    pub(super) async fn submit(
        &mut self,
        spec: ActionSpec,
        minimum_stamp_ns: u64,
        known_action: &mut Option<String>,
    ) -> Result<String, String> {
        let mut socket = self.socket.take().ok_or("Web提交已尝试，不得重复发送")?;
        let result = self
            .submit_once(&mut socket, &spec, minimum_stamp_ns, known_action)
            .await;
        // 后续观察使用原监督gRPC；不留无人消费的快照队列或后台读线程。
        let _ = tokio::time::timeout(Duration::from_millis(250), socket.close(None)).await;
        result
    }

    async fn submit_once(
        &self,
        socket: &mut Socket,
        spec: &ActionSpec,
        minimum_stamp_ns: u64,
        known_action: &mut Option<String>,
    ) -> Result<String, String> {
        if spec.payload_schema_id != robot_msg::schema_id::SCHEMA_SINGLE_JOINT_POSITION_PAYLOAD {
            return Err("Web提交仅支持既有单关节载荷，未发送".into());
        }
        let payload =
            flatbuffers::root::<robot_msg::msg::SingleJointPositionPayload>(&spec.payload)
                .map_err(|error| format!("单关节载荷无效：{error}"))?;
        if payload.hardware_id() != self.hardware_id.as_str() {
            return Err("Web任务硬件身份与原监督计划不同，未发送".into());
        }
        let ready = loop {
            let value = receive_json(socket).await?;
            if let Some(ready) = self.ready(&value, minimum_stamp_ns)? {
                break ready;
            }
            tokio::task::yield_now().await;
        };
        let request_id = uuidv7_compat();
        let request = json!({
            "type": "control", "operation": "move", "request_id": request_id,
            "bus_id": self.config.bus_id, "process_id": ready.process_id,
            "revision": ready.revision.to_string(), "joint": payload.joint_name(),
            "target_position_rad": payload.target_position_rad(), "duration_ms": payload.duration_ms(),
        });
        let wire = serde_json::to_string(&request).map_err(|error| error.to_string())?;
        if wire.len() > 1024 {
            return Err("Web请求超过现有1KiB消息上限，未发送".into());
        }
        println!(
            "WEB_SINGLE_JOINT_REQUEST request_id={request_id} bus_id={} revision={}",
            self.config.bus_id, ready.revision
        );
        socket
            .send(Message::Text(wire.into()))
            .await
            .map_err(|error| format!("Web发送结果未知，不重试：{error}"))?;
        loop {
            let value = receive_json(socket).await?;
            if value["type"] == "control_result" {
                return self.receipt(&value, &request_id, &ready, known_action);
            }
            let bus = self.bus(&value)?;
            if bus["control"]["process_id"] != ready.process_id {
                return Err("Web进程已变化，提交结果未知，不重试".into());
            }
            tokio::task::yield_now().await;
        }
    }

    fn bus<'a>(&self, value: &'a Value) -> Result<&'a Value, String> {
        let buses = value["buses"].as_array().ok_or("Web快照缺少总线列表")?;
        let matching: Vec<_> = buses
            .iter()
            .filter(|bus| bus["id"] == self.config.bus_id)
            .collect();
        let [bus] = matching.as_slice() else {
            return Err("Web快照总线缺失或重复".into());
        };
        if bus["robot_id"] != self.robot_id
            || bus["hardware_id"] != self.hardware_id
            || bus["layout_id"] != self.layout_id
        {
            return Err("Web快照robot/hardware/layout与原监督计划不同".into());
        }
        Ok(bus)
    }

    fn ready(&self, value: &Value, minimum_stamp_ns: u64) -> Result<Option<Ready>, String> {
        let bus = self.bus(value)?;
        let control = &bus["control"];
        if control["enabled"] != true || control["busy"] != false {
            return Err("Web控制未开放或已有进行中/未知任务，未发送".into());
        }
        if bus["fault_code"].as_u64().is_some_and(|code| code != 0) {
            return Err("Web报告故障，未发送任务".into());
        }
        let stamp = bus["stamp_ns"]
            .as_str()
            .and_then(|stamp| stamp.parse::<u64>().ok());
        if bus["data_status"] != "fresh"
            || bus["mode"] != "Enabled"
            || bus["safety_state"] != "TorqueOn"
            || bus["output_mode"] != "Active"
            || bus["control_link"] != "Ok"
            || bus["fault_code"] != 0
            || stamp.is_none_or(|stamp| stamp < minimum_stamp_ns)
        {
            return Ok(None);
        }
        let process_id = control["process_id"].as_str().ok_or("Web进程身份缺失")?;
        robot_rpc::auth::validate_request_id(process_id).map_err(|error| error.to_string())?;
        let revision = control["revision"]
            .as_str()
            .and_then(|version| version.parse::<u64>().ok())
            .ok_or("Web版本无效")?;
        Ok(Some(Ready {
            process_id: process_id.into(),
            revision,
        }))
    }

    fn receipt(
        &self,
        value: &Value,
        request_id: &str,
        ready: &Ready,
        known_action: &mut Option<String>,
    ) -> Result<String, String> {
        if value["request_id"] != request_id
            || value["bus_id"] != self.config.bus_id
            || value["operation"] != "move"
        {
            return Err("Web回执身份不符，结果未知，不重试".into());
        }
        if value["status"] == "rejected" {
            return Err(format!("Web拒绝任务：{}", value["message"]));
        }
        let next_revision = ready.revision.checked_add(1).ok_or("Web版本耗尽")?;
        let expected_revision = next_revision.to_string();
        if value["revision"].as_str() != Some(expected_revision.as_str()) {
            return Err("Web回执版本不符，结果未知，不重试".into());
        }
        let action = value["action_id"]
            .as_str()
            .ok_or("Web回执无任务标识，结果未知")?;
        robot_rpc::auth::validate_request_id(action)
            .map_err(|error| format!("Web任务标识无效：{error}"))?;
        *known_action = Some(action.into());
        if value["status"] != "accepted" {
            return Err(format!("Web任务结果未知，不重试：{}", value["message"]));
        }
        println!("WEB_SINGLE_JOINT_ACCEPTED request_id={request_id} action_id={action}");
        Ok(action.into())
    }
}

async fn receive_json(socket: &mut Socket) -> Result<Value, String> {
    loop {
        match socket.next().await {
            Some(Ok(Message::Text(text))) => {
                return serde_json::from_str(&text)
                    .map_err(|error| format!("Web消息无效：{error}"));
            }
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => tokio::task::yield_now().await,
            Some(Ok(_)) | None => return Err("Web连接关闭或返回非文本消息；不重试任务".into()),
            Some(Err(error)) => return Err(format!("Web连接失败，结果未知：{error}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_hdr_async;
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

    fn snapshot(robot: &str) -> Value {
        json!({"buses": [{
            "id": "bus1", "robot_id": robot, "hardware_id": "arm", "layout_id": "42",
            "data_status": "fresh", "stamp_ns": "100", "mode": "Enabled",
            "safety_state": "TorqueOn", "output_mode": "Active", "control_link": "Ok", "fault_code": 0,
            "control": {"enabled": true, "busy": false, "process_id": uuidv7_compat(), "revision": "8"}
        }]})
    }

    fn spec() -> ActionSpec {
        let mut builder = flatbuffers::FlatBufferBuilder::new();
        let hardware = builder.create_string("arm");
        let joint = builder.create_string("wrist");
        let root = robot_msg::msg::SingleJointPositionPayload::create(
            &mut builder,
            &robot_msg::msg::SingleJointPositionPayloadArgs {
                hardware_id: Some(hardware),
                joint_name: Some(joint),
                target_position_rad: 0.25,
                duration_ms: 20000,
            },
        );
        builder.finish(root, None);
        ActionSpec {
            kind: robot_rpc::client::dual_arm_workflow::ActionKindWire::Scripted,
            payload_schema_id: robot_msg::schema_id::SCHEMA_SINGLE_JOINT_POSITION_PAYLOAD,
            payload: builder.finished_data().to_vec(),
        }
    }

    async fn fixture(
        robot: &str,
        outcome: &str,
    ) -> (
        Session,
        Arc<AtomicUsize>,
        String,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let expected_origin = format!("http://{address}");
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        let initial = snapshot(robot);
        let outcome = outcome.to_owned();
        let action_id = uuidv7_compat();
        let server_action = action_id.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            // 握手回调由tungstenite固定返回Response错误，不能在此改变第三方签名。
            #[allow(clippy::result_large_err)]
            let verify_origin = |request: &Request, response: Response| {
                assert_eq!(
                    request.headers()[header::ORIGIN].to_str().unwrap(),
                    expected_origin
                );
                Ok(response)
            };
            let mut socket = accept_hdr_async(stream, verify_origin).await.unwrap();
            socket
                .send(Message::Text(initial.to_string().into()))
                .await
                .unwrap();
            while let Some(Ok(message)) = socket.next().await {
                match message {
                    Message::Text(text) => {
                        observed.fetch_add(1, Ordering::SeqCst);
                        let request: Value = serde_json::from_str(&text).unwrap();
                        assert_eq!(request["operation"], "move");
                        assert_eq!(request["joint"], "wrist");
                        assert_eq!(request["target_position_rad"], 0.25);
                        assert_eq!(request["duration_ms"], 20000);
                        assert_eq!(request["revision"], "8");
                        assert_eq!(
                            request["process_id"],
                            initial["buses"][0]["control"]["process_id"]
                        );
                        assert_ne!(request["request_id"], server_action);
                        let reply = json!({"type":"control_result", "request_id":request["request_id"],
                            "bus_id":"bus1", "operation":"move", "revision":"9", "status":outcome,
                            "action_id":server_action, "message":"软件夹具"});
                        socket
                            .send(Message::Text(reply.to_string().into()))
                            .await
                            .unwrap();
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        });
        let config = Config::new(&format!("ws://{address}/api/ws"), "bus1").unwrap();
        let client = Session::connect(config, "robot", "arm", 42).await.unwrap();
        (client, calls, action_id, server)
    }

    async fn finish(server: tokio::task::JoinHandle<()>) {
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
    }

    #[test]
    fn web_url_and_stale_snapshot_are_rejected_before_submission() {
        assert!(Config::new("ws://10.0.1.40:8080/api/ws", "bus1").is_ok());
        for url in [
            "wss://10.0.1.40/api/ws",
            "ws://0.0.0.0:8080/api/ws",
            "ws://8.8.8.8/api/ws",
            "ws://127.0.0.1/api/ws?x=1",
            "ws://user@127.0.0.1/api/ws",
        ] {
            assert!(Config::new(url, "bus1").is_err(), "{url}");
        }
        let session = Session {
            socket: None,
            config: Config::new("ws://127.0.0.1:8080/api/ws", "bus1").unwrap(),
            robot_id: "robot".into(),
            hardware_id: "arm".into(),
            layout_id: "42".into(),
            enable_attempted: false,
        };
        assert!(session.ready(&snapshot("robot"), 101).unwrap().is_none());
        for field in [
            "mode",
            "safety_state",
            "output_mode",
            "control_link",
            "data_status",
        ] {
            let mut value = snapshot("robot");
            value["buses"][0][field] = json!("waiting");
            assert!(session.ready(&value, 100).unwrap().is_none(), "{field}");
        }
        for field in ["robot_id", "hardware_id", "layout_id"] {
            let mut value = snapshot("robot");
            value["buses"][0][field] = json!("wrong");
            assert!(session.ready(&value, 100).is_err(), "{field}");
        }
    }

    #[tokio::test]
    async fn successful_web_submission_uses_server_action_once_and_closes() {
        let (mut session, calls, action_id, server) = fixture("robot", "accepted").await;
        let mut known = None;
        let result =
            tokio::time::timeout(CLIENT_HARD_TIMEOUT, session.submit(spec(), 100, &mut known))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(result, action_id);
        assert_eq!(known.as_deref(), Some(action_id.as_str()));
        assert!(session.submit(spec(), 100, &mut known).await.is_err());
        finish(server).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn web_enable_and_move_share_one_connection_and_each_send_once() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(MaybeTlsStream::Plain(stream))
                .await
                .unwrap();
            let mut value = snapshot("robot");
            value["buses"][0]["mode"] = json!("Safe");
            value["buses"][0]["safety_state"] = json!("TorqueOffConfirmed");
            value["buses"][0]["control"]["can_enable"] = json!(true);
            socket
                .send(Message::Text(value.to_string().into()))
                .await
                .unwrap();
            let enable = receive_json(&mut socket).await.unwrap();
            assert_eq!(enable["operation"], "enable");
            assert_eq!(enable["revision"], "8");
            let reply = json!({"type":"control_result", "request_id":enable["request_id"],
                "bus_id":"bus1", "operation":"enable", "revision":"9", "status":"accepted"});
            socket
                .send(Message::Text(reply.to_string().into()))
                .await
                .unwrap();
            value["buses"][0]["mode"] = json!("Enabled");
            value["buses"][0]["safety_state"] = json!("TorqueOn");
            value["buses"][0]["stamp_ns"] = json!("101");
            value["buses"][0]["control"]["revision"] = json!("9");
            socket
                .send(Message::Text(value.to_string().into()))
                .await
                .unwrap();
            let request = receive_json(&mut socket).await.unwrap();
            assert_eq!(request["operation"], "move");
            assert_eq!(request["revision"], "9");
            assert_ne!(request["request_id"], enable["request_id"]);
            let reply = json!({"type":"control_result", "request_id":request["request_id"],
                "bus_id":"bus1", "operation":"move", "revision":"10", "status":"accepted",
                "action_id":uuidv7_compat()});
            socket
                .send(Message::Text(reply.to_string().into()))
                .await
                .unwrap();
            assert!(matches!(socket.next().await, Some(Ok(Message::Close(_)))));
        });
        let config = Config::new(&format!("ws://{address}/api/ws"), "bus1").unwrap();
        let mut session = Session::connect(config, "robot", "arm", 42).await.unwrap();
        tokio::time::timeout(CLIENT_HARD_TIMEOUT, session.enable(100))
            .await
            .unwrap()
            .unwrap();
        assert!(session.enable(100).await.is_err());
        let mut known = None;
        let action =
            tokio::time::timeout(CLIENT_HARD_TIMEOUT, session.submit(spec(), 101, &mut known))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(known.as_deref(), Some(action.as_str()));
        finish(server).await;
    }

    #[tokio::test]
    async fn unknown_web_enable_closes_without_move_or_retry() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(MaybeTlsStream::Plain(stream))
                .await
                .unwrap();
            let mut value = snapshot("robot");
            value["buses"][0]["mode"] = json!("Safe");
            value["buses"][0]["safety_state"] = json!("TorqueOffConfirmed");
            value["buses"][0]["control"]["can_enable"] = json!(true);
            socket
                .send(Message::Text(value.to_string().into()))
                .await
                .unwrap();
            let request = receive_json(&mut socket).await.unwrap();
            assert_eq!(request["operation"], "enable");
            let reply = json!({"type":"control_result", "request_id":request["request_id"],
                "bus_id":"bus1", "operation":"enable", "revision":"9", "status":"unknown"});
            socket
                .send(Message::Text(reply.to_string().into()))
                .await
                .unwrap();
            assert!(!matches!(socket.next().await, Some(Ok(Message::Text(_)))));
        });
        let config = Config::new(&format!("ws://{address}/api/ws"), "bus1").unwrap();
        let mut session = Session::connect(config, "robot", "arm", 42).await.unwrap();
        assert!(
            tokio::time::timeout(CLIENT_HARD_TIMEOUT, session.enable(100))
                .await
                .unwrap()
                .is_err()
        );
        assert!(session.enable(100).await.is_err());
        assert!(session.submit(spec(), 100, &mut None).await.is_err());
        finish(server).await;
    }

    #[tokio::test]
    async fn enable_budget_cancellation_drops_socket_and_never_retries() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (sent, received) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(MaybeTlsStream::Plain(stream))
                .await
                .unwrap();
            let mut value = snapshot("robot");
            value["buses"][0]["mode"] = json!("Safe");
            value["buses"][0]["safety_state"] = json!("TorqueOffConfirmed");
            value["buses"][0]["control"]["can_enable"] = json!(true);
            socket
                .send(Message::Text(value.to_string().into()))
                .await
                .unwrap();
            assert_eq!(
                receive_json(&mut socket).await.unwrap()["operation"],
                "enable"
            );
            // 确定请求已发出后取消客户端future，等价预算超时的丢弃语义，无脆弱sleep。
            sent.send(()).unwrap();
            assert!(!matches!(socket.next().await, Some(Ok(Message::Text(_)))));
        });
        let config = Config::new(&format!("ws://{address}/api/ws"), "bus1").unwrap();
        let mut session = Session::connect(config, "robot", "arm", 42).await.unwrap();
        {
            let enable = session.enable(100);
            tokio::pin!(enable);
            tokio::select! {
                result = &mut enable => panic!("未回执不得完成：{result:?}"),
                result = received => result.unwrap(),
            }
        }
        assert!(session.enable(100).await.is_err());
        assert!(session.submit(spec(), 100, &mut None).await.is_err());
        finish(server).await;
    }

    #[tokio::test]
    async fn exhausted_revision_never_sends_enable() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(MaybeTlsStream::Plain(stream))
                .await
                .unwrap();
            let mut value = snapshot("robot");
            value["buses"][0]["mode"] = json!("Safe");
            value["buses"][0]["safety_state"] = json!("TorqueOffConfirmed");
            value["buses"][0]["control"]["can_enable"] = json!(true);
            value["buses"][0]["control"]["revision"] = json!(u64::MAX.to_string());
            socket
                .send(Message::Text(value.to_string().into()))
                .await
                .unwrap();
            assert!(!matches!(socket.next().await, Some(Ok(Message::Text(_)))));
        });
        let config = Config::new(&format!("ws://{address}/api/ws"), "bus1").unwrap();
        let mut session = Session::connect(config, "robot", "arm", 42).await.unwrap();
        assert!(
            tokio::time::timeout(CLIENT_HARD_TIMEOUT, session.enable(100))
                .await
                .unwrap()
                .unwrap_err()
                .contains("版本耗尽")
        );
        finish(server).await;
    }

    #[tokio::test]
    async fn wrong_web_identity_never_sends_move() {
        let (mut session, calls, _, server) = fixture("other", "accepted").await;
        let mut known = None;
        let result =
            tokio::time::timeout(CLIENT_HARD_TIMEOUT, session.submit(spec(), 100, &mut known))
                .await
                .unwrap();
        assert!(result.unwrap_err().contains("robot/hardware/layout"));
        assert!(known.is_none());
        finish(server).await;
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn unknown_web_ack_keeps_action_for_original_finalizer_without_resending() {
        let (mut session, calls, action_id, server) = fixture("robot", "unknown").await;
        let mut known = None;
        let result =
            tokio::time::timeout(CLIENT_HARD_TIMEOUT, session.submit(spec(), 100, &mut known))
                .await
                .unwrap();
        assert!(result.unwrap_err().contains("结果未知"));
        assert_eq!(known.as_deref(), Some(action_id.as_str()));
        assert!(session.submit(spec(), 100, &mut known).await.is_err());
        finish(server).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
