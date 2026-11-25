//! 网页模型修正单独保存；舵机校准由本机单关节维护入口执行。

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use axum::extract::{DefaultBodyLimit, State, rejection::JsonRejection};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::Semaphore;

const FILE_NAME: &str = "model-calibration.json";
const SOCKET: &str = "/run/robot-joint-calibration/session.sock";
const MAX_FILE: u64 = 8192;
const MAX_REPLY: u64 = 8192;

#[derive(Clone)]
struct Calibration {
    directory: Arc<PathBuf>,
    socket: Arc<PathBuf>,
    requests: Arc<Semaphore>,
    controlled_buses: Vec<String>,
    starting: Arc<Semaphore>,
}

pub(super) fn router(controlled_buses: Vec<String>) -> Router {
    let directory = std::env::var_os("STATE_DIRECTORY")
        .map_or_else(|| PathBuf::from("/var/lib/robot-status"), PathBuf::from);
    routes(directory, PathBuf::from(SOCKET), controlled_buses)
}

fn routes(directory: PathBuf, socket: PathBuf, controlled_buses: Vec<String>) -> Router {
    Router::new()
        .route("/api/calibration/model", get(get_model).put(put_model))
        .route(
            "/api/calibration/motor",
            get(motor_status).post(motor_command),
        )
        .route("/api/calibration/motor/start", post(start_motor))
        .layer(DefaultBodyLimit::max(1024))
        .with_state(Calibration {
            directory: Arc::new(directory),
            socket: Arc::new(socket),
            requests: Arc::new(Semaphore::new(8)),
            controlled_buses,
            starting: Arc::new(Semaphore::new(1)),
        })
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged, deny_unknown_fields)]
enum Adjustment {
    Angle { sign: i8, offset: f64 },
    Gripper { motor_range: [f64; 2] },
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Model {
    overrides: BTreeMap<String, Adjustment>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Change {
    joint: String,
    // 缺少字段属于错误；只有显式传 null 才恢复这一关节的默认值。
    #[serde(deserialize_with = "Option::deserialize")]
    value: Option<Adjustment>,
}

#[derive(Deserialize)]
struct PoseModel {
    nodes: Vec<PoseNode>,
}

#[derive(Deserialize)]
struct PoseNode {
    bus: Option<String>,
    joint: Option<String>,
    range: Option<[f64; 2]>,
}

static POSE: LazyLock<PoseModel> = LazyLock::new(|| {
    serde_json::from_str(include_str!("pose_assets/model.json")).expect("内置姿态模型须能解析")
});

fn validate(joint: &str, value: Option<&Adjustment>) -> Result<(), &'static str> {
    let node = POSE
        .nodes
        .iter()
        .find(|node| node.joint.as_deref() == Some(joint))
        .ok_or("关节不在可视化模型中")?;
    let supported = match value {
        None => true,
        Some(Adjustment::Angle { sign, offset }) => {
            node.range.is_none() && matches!(sign, -1 | 1) && offset.is_finite()
        }
        Some(Adjustment::Gripper {
            motor_range: [closed, open],
        }) => {
            let span = (open - closed).abs();
            node.range.is_some() && span.is_finite() && span > 0.0
        }
    };
    supported
        .then_some(())
        .ok_or("关节类型、方向、零偏或夹爪端点无效")
}

fn read_model(directory: &Path) -> io::Result<Model> {
    let file = match File::open(directory.join(FILE_NAME)) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Model::default()),
        result => result?,
    };
    let mut bytes = Vec::new();
    file.take(MAX_FILE + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FILE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "模型校准文件超过大小上限",
        ));
    }
    let model: Model = serde_json::from_slice(&bytes)?;
    for (joint, adjustment) in &model.overrides {
        validate(joint, Some(adjustment))
            .map_err(|message| io::Error::new(io::ErrorKind::InvalidData, message))?;
    }
    Ok(model)
}

fn save_model(directory: &Path, model: &Model) -> io::Result<()> {
    let temporary = directory.join(format!(".{FILE_NAME}.{}", uuid::Uuid::now_v7()));
    let mut file = File::create_new(&temporary)?;
    let result = (|| {
        serde_json::to_writer(&mut file, model)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temporary, directory.join(FILE_NAME))?;
        File::open(directory)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn error(status: StatusCode, message: impl AsRef<str>) -> Response {
    (status, Json(json!({"ok":false,"error":message.as_ref()}))).into_response()
}

async fn get_model(State(state): State<Calibration>) -> Response {
    model_request(state, None).await
}

async fn put_model(
    State(state): State<Calibration>,
    headers: HeaderMap,
    change: Result<Json<Change>, JsonRejection>,
) -> Response {
    if !super::same_origin(&headers) {
        return error(StatusCode::FORBIDDEN, "模型校准需要同源请求");
    }
    let Json(change) = match change {
        Ok(value) => value,
        Err(cause) => return error(cause.status(), cause.body_text()),
    };
    if let Err(message) = validate(&change.joint, change.value.as_ref()) {
        return error(StatusCode::BAD_REQUEST, message);
    }
    model_request(state, Some(change)).await
}

async fn model_request(state: Calibration, change: Option<Change>) -> Response {
    let Ok(permit) = state.requests.try_acquire_owned() else {
        return error(StatusCode::TOO_MANY_REQUESTS, "校准请求过多");
    };
    let result = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let Some(change) = change else {
            return read_model(&state.directory);
        };
        // 锁定稳定的目录 inode，原子替换文件、并行浏览器及进程交接共用同一把锁。
        let directory = File::open(&*state.directory)?;
        directory.lock()?;
        let mut model = read_model(&state.directory)?;
        if let Some(value) = change.value {
            model.overrides.insert(change.joint, value);
        } else {
            model.overrides.remove(&change.joint);
        }
        save_model(&state.directory, &model)?;
        Ok(model)
    })
    .await;
    match result {
        Ok(Ok(model)) => Json(model).into_response(),
        Ok(Err(cause)) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("模型校准文件读写失败：{cause}"),
        ),
        Err(cause) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("模型校准读写任务退出：{cause}"),
        ),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StartMotor {
    bus: String,
    joint: String,
    supported: bool,
}

async fn start_motor(
    State(state): State<Calibration>,
    headers: HeaderMap,
    command: Result<Json<StartMotor>, JsonRejection>,
) -> Response {
    if !super::same_origin(&headers) {
        return error(StatusCode::FORBIDDEN, "启动单电机校准需要同源请求");
    }
    let Json(command) = match command {
        Ok(value) => value,
        Err(cause) => return error(cause.status(), cause.body_text()),
    };
    if !state.controlled_buses.contains(&command.bus) {
        return error(StatusCode::FORBIDDEN, "这条总线未开放网页控制");
    }
    if !command.supported {
        return error(
            StatusCode::BAD_REQUEST,
            "请先确认机械臂有支撑、现场可手动移动关节",
        );
    }
    if !POSE.nodes.iter().any(|node| {
        node.bus.as_deref() == Some(&command.bus) && node.joint.as_deref() == Some(&command.joint)
    }) {
        return error(StatusCode::BAD_REQUEST, "所选总线没有这个可校准的位置关节");
    }
    let Ok(_permit) = state.starting.try_acquire() else {
        return error(StatusCode::CONFLICT, "已有校准交接正在进行，请读取维护状态");
    };
    let packet = json!({"action":"start","bus":command.bus,"joint":command.joint});
    let socket = state.socket.with_file_name("start.sock");
    let result = tokio::time::timeout(Duration::from_secs(75), exchange(&socket, &packet)).await;
    match result {
        Ok(Ok(reply)) if reply["ok"] == true => {
            if reply["bus"] != command.bus
                || reply["joint"] != command.joint
                || reply["session"]
                    .as_str()
                    .is_none_or(|id| uuid::Uuid::parse_str(id).is_err())
                || !matches!(
                    reply["phase"].as_str(),
                    Some("ready" | "centered" | "sampled1" | "sampled2" | "recovery")
                )
            {
                return error(
                    StatusCode::BAD_GATEWAY,
                    "开启结果与所选电机不符，请读取维护状态",
                );
            }
            Json(reply).into_response()
        }
        Ok(Ok(reply)) => (StatusCode::CONFLICT, Json(reply)).into_response(),
        Ok(Err(cause)) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("维护入口通讯失败：{cause}"),
        ),
        Err(_) => error(
            StatusCode::GATEWAY_TIMEOUT,
            "维护交接回执超时，请读取维护状态",
        ),
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum MotorAction {
    Center,
    Sample1,
    Sample2,
    Save,
    Cancel,
    Restore,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct MotorCommand {
    bus: String,
    action: MotorAction,
    session: String,
}

async fn motor_status(State(state): State<Calibration>) -> Response {
    motor_request(&state, json!({"action":"status","session":""})).await
}

async fn motor_command(
    State(state): State<Calibration>,
    headers: HeaderMap,
    command: Result<Json<MotorCommand>, JsonRejection>,
) -> Response {
    if !super::same_origin(&headers) {
        return error(StatusCode::FORBIDDEN, "舵机校准需要已开启控制的同源页面");
    }
    let Json(command) = match command {
        Ok(value) => value,
        Err(cause) => return error(cause.status(), cause.body_text()),
    };
    if !state.controlled_buses.contains(&command.bus) {
        return error(StatusCode::FORBIDDEN, "这条总线未开放网页控制");
    }
    if uuid::Uuid::parse_str(&command.session).is_err() {
        return error(StatusCode::BAD_REQUEST, "维护会话标识无效");
    }
    motor_request(&state, json!(command)).await
}

async fn motor_request(state: &Calibration, command: Value) -> Response {
    let Ok(_permit) = state.requests.try_acquire() else {
        return error(StatusCode::TOO_MANY_REQUESTS, "校准请求过多");
    };
    let reading = command["action"] == "status";
    let result =
        tokio::time::timeout(Duration::from_secs(3), exchange(&state.socket, &command)).await;
    match result {
        Ok(Ok(reply)) if reply["ok"] == true => Json(reply).into_response(),
        Ok(Ok(reply)) => (StatusCode::CONFLICT, Json(reply)).into_response(),
        Ok(Err(cause))
            if matches!(
                cause.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            if reading {
                return handoff_status(state.socket.with_file_name("handoff.json")).await;
            }
            error(
                StatusCode::SERVICE_UNAVAILABLE,
                "当前维护会话未连接，请读取维护状态",
            )
        }
        Ok(Err(cause)) => error(
            StatusCode::BAD_GATEWAY,
            format!("维护会话通讯失败：{cause}"),
        ),
        Err(_) => error(
            StatusCode::GATEWAY_TIMEOUT,
            "维护会话响应超时，请重新读取状态",
        ),
    }
}

#[derive(Deserialize, Serialize)]
struct Handoff {
    bus: String,
    joint: String,
    phase: String,
    error: Option<String>,
    calibration_result: Option<String>,
}

async fn handoff_status(path: PathBuf) -> Response {
    // 原子交接记录可在释放串口、恢复保护待机期间读取，读状态不启动维护进程。
    let read = tokio::task::spawn_blocking(move || -> io::Result<Handoff> {
        let file = File::open(path)?;
        let mut bytes = Vec::new();
        file.take(MAX_REPLY + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_REPLY {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "交接记录过大"));
        }
        Ok(serde_json::from_slice(&bytes)?)
    })
    .await;
    match read {
        Ok(Ok(row)) => Json(json!({"ok":true,"bus":row.bus,"joint":row.joint,
            "phase":row.phase,"error":row.error,"calibration_result":row.calibration_result}))
        .into_response(),
        Ok(Err(cause)) if cause.kind() == io::ErrorKind::NotFound => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"ok":false,"phase":"idle","error":"现场维护会话尚未开启"})),
        )
            .into_response(),
        _ => error(StatusCode::BAD_GATEWAY, "无法读取维护交接记录"),
    }
}

async fn exchange(socket: &Path, command: &Value) -> io::Result<Value> {
    let mut stream = UnixStream::connect(socket).await?;
    let mut request = serde_json::to_vec(command)?;
    request.push(b'\n');
    stream.write_all(&request).await?;
    let mut reply = Vec::new();
    BufReader::new(stream.take(MAX_REPLY + 1))
        .read_until(b'\n', &mut reply)
        .await?;
    if reply.len() as u64 > MAX_REPLY || reply.last() != Some(&b'\n') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "维护会话回包过大或不完整",
        ));
    }
    let reply: Value = serde_json::from_slice(&reply)?;
    if !reply["ok"].is_boolean() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "维护会话回包缺少结果",
        ));
    }
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use tempfile::tempdir;
    use tokio::net::UnixListener;
    use tower::ServiceExt;

    async fn call(
        app: Router,
        method: &str,
        resource: &str,
        body: Value,
        origin: &str,
    ) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(method)
            .uri(format!("/api/calibration/{resource}"))
            .header("host", "robot.test")
            .header("origin", origin)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 16_384).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn model_updates_survive_restart_and_revert_one_joint_only() {
        let directory = tempdir().unwrap();
        let new_app = || {
            routes(
                directory.path().into(),
                directory.path().join("sock"),
                vec![],
            )
        };
        let app = new_app();
        let origin = "https://robot.test";
        let angle = json!({"joint":"right_arm_elbow_flex","value":{"sign":-1,"offset":0.4}});
        let gripper = json!({"joint":"left_arm_gripper","value":{"motor_range":[0.9,0.3]}});
        let (a, b) = tokio::join!(
            call(app.clone(), "PUT", "model", angle.clone(), origin),
            call(new_app(), "PUT", "model", gripper.clone(), origin)
        );
        assert_eq!(a.0, StatusCode::OK);
        assert_eq!(b.0, StatusCode::OK);
        let (_, reloaded) = call(new_app(), "GET", "model", Value::Null, origin).await;
        assert_eq!(
            reloaded["overrides"]["right_arm_elbow_flex"],
            angle["value"]
        );
        assert_eq!(reloaded["overrides"]["left_arm_gripper"], gripper["value"]);
        let (_, reverted) = call(
            app,
            "PUT",
            "model",
            json!({"joint":"right_arm_elbow_flex","value":null}),
            origin,
        )
        .await;
        assert_eq!(
            reverted["overrides"],
            json!({"left_arm_gripper":gripper["value"]})
        );
    }

    #[tokio::test]
    async fn invalid_inputs_cannot_replace_existing_model() {
        let directory = tempdir().unwrap();
        let app = routes(
            directory.path().into(),
            directory.path().join("sock"),
            vec![],
        );
        let valid = json!({"joint":"head_motor_1","value":{"sign":1,"offset":0.3}});
        assert_eq!(
            call(
                app.clone(),
                "PUT",
                "model",
                valid.clone(),
                "https://elsewhere.test"
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        for invalid in [
            json!({"joint":"../state","value":null}),
            json!({"joint":"head_motor_1"}),
            json!({"joint":"head_motor_1","value":{"sign":0,"offset":0.5}}),
            json!({"joint":"head_motor_1","value":{"motor_range":[1,2]}}),
            json!({"joint":"left_arm_gripper","value":{"motor_range":[1,1]}}),
            json!({"joint":"head_motor_1","value":{"sign":1,"offset":0,"path":"foo"}}),
        ] {
            assert!(
                call(app.clone(), "PUT", "model", invalid, "http://robot.test")
                    .await
                    .0
                    .is_client_error()
            );
        }
        assert!(!directory.path().join(FILE_NAME).exists());
        fs::write(directory.path().join(FILE_NAME), b"corrupt").unwrap();
        assert_eq!(
            call(app, "PUT", "model", valid, "http://robot.test")
                .await
                .0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            fs::read(directory.path().join(FILE_NAME)).unwrap(),
            b"corrupt"
        );
    }

    #[tokio::test]
    async fn motor_requests_use_one_connection_and_only_allowed_operations() {
        let directory = tempdir().unwrap();
        let socket = directory.path().join("session.sock");
        let origin = "https://robot.test";
        let app = routes(directory.path().into(), socket.clone(), vec!["bus1".into()]);
        let id = uuid::Uuid::now_v7().to_string();
        let command = json!({"bus":"bus1","action":"sample1","session":id});
        assert_eq!(
            call(app.clone(), "GET", "motor", Value::Null, origin)
                .await
                .0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        let listener = UnixListener::bind(&socket).unwrap();
        let no_control = routes(directory.path().into(), socket.clone(), vec![]);
        assert_eq!(
            call(no_control, "POST", "motor", command.clone(), origin)
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(
                app.clone(),
                "POST",
                "motor",
                command.clone(),
                "http://other.test"
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        assert!(
            call(
                app.clone(),
                "POST",
                "motor",
                json!({"bus":"bus1","action":"enable","session":id}),
                origin
            )
            .await
            .0
            .is_client_error()
        );
        assert_eq!(
            call(
                app.clone(),
                "POST",
                "motor",
                json!({"bus":"bus1","action":"sample1","session":""}),
                origin
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        for bus in ["bus2", "../bus1"] {
            let unsupported = json!({"bus":bus,"action":"sample1","session":id});
            assert_eq!(
                call(app.clone(), "POST", "motor", unsupported, origin)
                    .await
                    .0,
                StatusCode::FORBIDDEN
            );
        }
        let expected = command.clone();
        let worker = tokio::spawn(async move {
            for request in [json!({"action":"status","session":""}), expected] {
                let (stream, _) = listener.accept().await.unwrap();
                let mut stream = BufReader::new(stream);
                let mut line = String::new();
                stream.read_line(&mut line).await.unwrap();
                assert_eq!(serde_json::from_str::<Value>(&line).unwrap(), request);
                stream
                    .get_mut()
                    .write_all(b"{\"ok\":true,\"phase\":\"ready\"}\n")
                    .await
                    .unwrap();
            }
        });
        assert_eq!(
            call(app.clone(), "GET", "motor", Value::Null, origin)
                .await
                .1["phase"],
            "ready"
        );
        assert_eq!(
            call(app, "POST", "motor", command, origin).await.0,
            StatusCode::OK
        );
        worker.await.unwrap();
    }

    #[tokio::test]
    async fn start_only_the_confirmed_position_joint_and_await_real_session() {
        let directory = tempdir().unwrap();
        let socket = directory.path().join("session.sock");
        let app = routes(directory.path().into(), socket.clone(), vec!["bus2".into()]);
        let origin = "http://robot.test";
        let joint = "right_arm_wrist_roll";
        let command = json!({"bus":"bus2", "joint":joint, "supported":true});
        for (body, caller, status) in [
            (command.clone(), "http://other.test", StatusCode::FORBIDDEN),
            (
                json!({"bus":"bus1","joint":"head_motor_1","supported":true}),
                origin,
                StatusCode::FORBIDDEN,
            ),
            (
                json!({"bus":"bus2","joint":"left_arm_wrist_roll","supported":true}),
                origin,
                StatusCode::BAD_REQUEST,
            ),
            (
                json!({"bus":"bus2","joint":"wheel_front","supported":true}),
                origin,
                StatusCode::BAD_REQUEST,
            ),
            (
                json!({"bus":"bus2","joint":joint,"supported":false}),
                origin,
                StatusCode::BAD_REQUEST,
            ),
        ] {
            assert_eq!(
                call(app.clone(), "POST", "motor/start", body, caller)
                    .await
                    .0,
                status
            );
        }
        let listener = UnixListener::bind(socket.with_file_name("start.sock")).unwrap();
        let pending = tokio::spawn(call(
            app.clone(),
            "POST",
            "motor/start",
            command.clone(),
            origin,
        ));
        let (stream, _) = listener.accept().await.unwrap();
        let mut stream = BufReader::new(stream);
        let mut line = String::new();
        stream.read_line(&mut line).await.unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&line).unwrap(),
            json!({"action":"start","bus":"bus2","joint":joint})
        );
        assert_eq!(
            call(app.clone(), "POST", "motor/start", command, origin)
                .await
                .0,
            StatusCode::CONFLICT
        );
        let ready = json!({"ok":true, "session":uuid::Uuid::now_v7().to_string(), "bus":"bus2", "joint":joint, "phase":"ready"});
        stream
            .get_mut()
            .write_all(format!("{ready}\n").as_bytes())
            .await
            .unwrap();
        assert_eq!(pending.await.unwrap(), (StatusCode::OK, ready));
        fs::write(socket.with_file_name("handoff.json"), br#"{"bus":"bus2","joint":"right_arm_wrist_roll","phase":"failed","error":"handoff failed","calibration_result":"saved"}"#).unwrap();
        let (status, report) = call(app, "GET", "motor", Value::Null, origin).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(report["phase"], "failed");
        assert_eq!(report["error"], "handoff failed");
        assert_eq!(report["calibration_result"], "saved");
    }

    #[tokio::test]
    async fn partial_and_oversized_maintenance_responses_are_invalid() {
        let directory = tempdir().unwrap();
        let socket = directory.path().join("session.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let worker = tokio::spawn(async move {
            for bytes in [
                b"{\"ok\":true}".to_vec(),
                vec![b'x'; usize::try_from(MAX_REPLY).unwrap() + 1],
            ] {
                let (stream, _) = listener.accept().await.unwrap();
                let mut stream = BufReader::new(stream);
                let mut line = String::new();
                stream.read_line(&mut line).await.unwrap();
                stream.get_mut().write_all(&bytes).await.unwrap();
            }
        });
        for _ in 0..2 {
            assert_eq!(
                exchange(&socket, &json!({"action":"status"}))
                    .await
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidData
            );
        }
        worker.await.unwrap();
    }
}
