//! Web控制的固定容量共享状态；复用状态轮询持有的gRPC连接，不启动机器人进程。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use flatbuffers::FlatBufferBuilder;
use serde::{Deserialize, Serialize};
use tonic::transport::Channel;

use super::command;
use super::{BusView, Source};
use crate::proto::robot_control_client::RobotControlClient;
use crate::proto::{
    Ack, ActionKindProto, AuthContext, CancelTaskCommand, ClearEStopCommand, CommandMeta,
    EStopCommand, InvokeTaskCommand, SetEnabledCommand, TaskHandle, TaskHandleRef, TaskState,
    TaskStatus,
};
use robot_core::time::RobotClock;

#[path = "status_base.rs"]
mod base;
pub(super) use base::wheel_slots as base_wheel_slots;

pub(super) const MAILBOX_CAPACITY: usize = 8;

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FileConfig {
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SourceConfig {
    pub motion_config: PathBuf,
    #[serde(default)]
    pub enable_alignment_tolerance_rad: Option<f64>,
    #[serde(default)]
    pub base_simulation: bool,
}

impl SourceConfig {
    pub(super) fn validate(&self) -> Result<(), String> {
        if self.base_simulation && !cfg!(feature = "test-base-command") {
            return Err("底盘控制仅接受显式test-base-command模拟构建".into());
        }
        if self
            .enable_alignment_tolerance_rad
            .is_some_and(|value| !value.is_finite() || value < 0.0)
        {
            return Err("使能原位对齐容差必须是有限非负rad值".into());
        }
        Ok(())
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct MotionConfig {
    pub max_duration_ms: u32,
    settle_timeout_ms: u32,
    stable_window_ms: u32,
    pub joints: Vec<JointLimit>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct JointLimit {
    pub(super) name: String,
    pub(super) min_position_rad: f64,
    pub(super) max_position_rad: f64,
    pub(super) max_velocity_rad_s: f64,
    pub(super) tolerance_rad: f64,
}

impl MotionConfig {
    pub(super) fn position_range(&self, name: &str) -> Option<[f64; 2]> {
        let joint = self.joints.iter().find(|joint| joint.name == name)?;
        Some([joint.min_position_rad, joint.max_position_rad])
    }

    #[allow(
        clippy::cast_possible_truncation,
        reason = "先检查f64有限及f32范围，再核对实际编码值"
    )]
    pub(super) fn check_hold_positions(
        &self,
        source: &Source,
        positions: &[f64],
    ) -> Result<(), String> {
        if positions.len() != source.layout.joints.len() {
            return Err("原位位置维度与配置不一致".into());
        }
        for (slot, &position) in positions.iter().enumerate() {
            if !position.is_finite() || position.abs() > f64::from(f32::MAX) {
                return Err("原位位置含非有限或不可表示值".into());
            }
            if source.mixed_raw && matches!(source.layout.motor_ids[slot], 7..=9) {
                continue;
            }
            let name = &source.layout.joints[slot];
            let limit = self
                .joints
                .iter()
                .find(|joint| &joint.name == name)
                .ok_or_else(|| format!("{name}缺少位置范围配置"))?;
            let (min, max) = hold_range(limit, source, slot)?;
            let range = min..=max;
            if !range.contains(&position) || !range.contains(&f64::from(position as f32)) {
                return Err(format!("{name}原位位置超出校准保持范围"));
            }
        }
        Ok(())
    }

    pub(super) fn load(path: &Path, source: &Source) -> Result<Self, String> {
        #[derive(Deserialize)]
        struct Document {
            s1: Section,
        }
        #[derive(Deserialize)]
        struct Section {
            single_joint: MotionConfig,
        }
        let text = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
        let config = toml::from_str::<Document>(&text)
            .map_err(|error| error.to_string())?
            .s1
            .single_joint;
        config.validate(source)?;
        Ok(config)
    }

    fn validate(&self, source: &Source) -> Result<(), String> {
        if self.max_duration_ms == 0
            || self.settle_timeout_ms == 0
            || self.stable_window_ms == 0
            || self.stable_window_ms >= self.settle_timeout_ms
        {
            return Err("运动时长及到位窗口配置无效".into());
        }
        let expected: Vec<_> = source
            .layout
            .joints
            .iter()
            .zip(&source.layout.motor_ids)
            .filter(|(_, id)| !source.mixed_raw || !(7..=9).contains(*id))
            .map(|(name, _)| name)
            .collect();
        if self.joints.len() != expected.len() {
            return Err("运动配置必须完整覆盖位置关节且不得包含轮子".into());
        }
        for (index, joint) in self.joints.iter().enumerate() {
            if !expected.contains(&&joint.name)
                || self.joints[..index]
                    .iter()
                    .any(|other| other.name == joint.name)
                || !joint.min_position_rad.is_finite()
                || !joint.max_position_rad.is_finite()
                || joint.min_position_rad >= joint.max_position_rad
                || !joint.max_velocity_rad_s.is_finite()
                || joint.max_velocity_rad_s <= 0.0
                || !joint.tolerance_rad.is_finite()
                || joint.tolerance_rad < 0.0
            {
                return Err(format!("运动配置关节或边界无效：{}", joint.name));
            }
            let slot = source
                .layout
                .joints
                .iter()
                .position(|name| name == &joint.name)
                .ok_or("运动配置缺少对应硬件槽位")?;
            hold_range(joint, source, slot)?;
        }
        Ok(())
    }
}

fn hold_range(joint: &JointLimit, source: &Source, slot: usize) -> Result<(f64, f64), String> {
    let tolerance = source
        .calibration_range_tolerances_raw
        .as_ref()
        .and_then(|values| values.get(slot))
        .copied()
        .unwrap_or(0);
    robot_adapter_feetech::convert::calibrated_hold_range(
        joint.min_position_rad,
        joint.max_position_rad,
        tolerance,
    )
    .ok_or_else(|| format!("{}原位边界必须来自可往返的校准raw端点", joint.name))
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Operation {
    Move,
    Cancel,
    Enable,
    Disable,
    Estop,
    ClearEstop,
    BaseHold,
    BaseStop,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WheelsRaw {
    front: i16,
    left: i16,
    right: i16,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
#[allow(
    clippy::struct_field_names,
    reason = "request_id是已冻结的WebSocket关联字段名"
)]
pub(super) struct Request {
    #[serde(rename = "type")]
    kind: String,
    pub request_id: String,
    pub bus_id: String,
    pub process_id: String,
    pub revision: String,
    pub operation: Operation,
    pub joint: Option<String>,
    pub target_position_rad: Option<f64>,
    pub duration_ms: Option<u32>,
    pub confirmation: Option<String>,
    pub hold_id: Option<String>,
    pub seq: Option<String>,
    pub wheels_raw: Option<WheelsRaw>,
}

impl Request {
    fn validate_shape(&self) -> Result<(), String> {
        if self.kind != "control"
            || self.request_id.is_empty()
            || self.request_id.len() > 80
            || !self
                .request_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
        {
            return Err("控制消息类型或请求标识无效".into());
        }
        match self.operation {
            Operation::BaseHold | Operation::BaseStop => {
                let hold = self.hold_id.as_deref().ok_or("底盘缺少hold_id")?;
                if crate::auth::validate_request_id(hold).is_err()
                    || self
                        .seq
                        .as_ref()
                        .and_then(|s| s.parse::<u64>().ok())
                        .is_none_or(|s| s == 0)
                    || self.has_motion()
                    || self.confirmation.is_some()
                    || (self.operation == Operation::BaseHold) != self.wheels_raw.is_some()
                {
                    return Err("底盘按住/停止标识、序号或参数无效".into());
                }
            }
            Operation::Move => {
                if self.joint.is_none()
                    || self.target_position_rad.is_none()
                    || self.duration_ms.is_none()
                    || self.confirmation.is_some()
                {
                    return Err("定位必须明确关节、rad目标和时长".into());
                }
            }
            Operation::ClearEstop => {
                if self.confirmation.as_deref() != Some("CONFIRM_CLEAR_ESTOP") || self.has_motion()
                {
                    return Err("解除急停需要独立的CONFIRM_CLEAR_ESTOP确认".into());
                }
            }
            _ => {
                if self.has_motion() || self.confirmation.is_some() {
                    return Err("该操作不接受位置参数或解除急停确认".into());
                }
            }
        }
        if !self.is_base()
            && (self.hold_id.is_some() || self.seq.is_some() || self.wheels_raw.is_some())
        {
            return Err("关节及使能操作不接受底盘参数".into());
        }
        Ok(())
    }

    pub(super) fn is_base(&self) -> bool {
        matches!(self.operation, Operation::BaseHold | Operation::BaseStop)
    }

    fn has_motion(&self) -> bool {
        self.joint.is_some() || self.target_position_rad.is_some() || self.duration_ms.is_some()
    }
}

#[derive(Clone, Serialize)]
pub(super) struct Reply {
    #[serde(rename = "type")]
    kind: &'static str,
    pub request_id: String,
    pub bus_id: String,
    pub operation: Option<Operation>,
    pub status: &'static str,
    pub message: String,
    pub action_id: Option<String>,
    pub revision: String,
    pub lease_deadline_ns: Option<String>,
}

impl Reply {
    pub(super) fn rejected(
        request: Option<&Request>,
        revision: u64,
        message: impl Into<String>,
    ) -> Self {
        Self {
            kind: "control_result",
            request_id: request.map_or_else(String::new, |r| r.request_id.clone()),
            bus_id: request.map_or_else(String::new, |r| r.bus_id.clone()),
            operation: request.map(|r| r.operation),
            status: "rejected",
            message: message.into(),
            action_id: None,
            revision: revision.to_string(),
            lease_deadline_ns: None,
        }
    }
}

pub(super) enum Envelope {
    Request {
        request: Box<Request>,
        reply: tokio::sync::oneshot::Sender<Reply>,
        connection_id: String,
        connected: Arc<AtomicBool>,
        received_ns: u64,
    },
    Disconnected {
        connection_id: String,
    },
}

#[derive(Clone, Serialize)]
pub(super) struct TaskView {
    pub action_id: String,
    pub state: &'static str,
    pub progress: Option<f32>,
    pub error: Option<String>,
}

impl TaskView {
    fn busy(&self) -> bool {
        matches!(self.state, "pending" | "executing" | "unknown")
    }
}

#[derive(Clone, Serialize)]
pub(super) struct View {
    pub enabled: bool,
    pub can_enable: bool,
    pub enable_reason: String,
    pub process_id: String,
    pub revision: String,
    pub busy: bool,
    pub max_duration_ms: u32,
    pub joints: Vec<JointLimit>,
    pub task: Option<TaskView>,
    pub last_result: Option<Reply>,
    pub base: BaseView,
}

#[derive(Clone, Default, Serialize)]
pub(super) struct BaseView {
    pub enabled: bool,
    pub can_hold: bool,
    #[cfg(feature = "test-base-command")]
    #[serde(skip_serializing_if = "Option::is_none")]
    directions: Option<&'static [base::Preset; 4]>,
    pub active: bool,
    pub hold_id: Option<String>,
    pub seq: Option<String>,
    pub lease_deadline_ns: Option<String>,
    pub reason: String,
}

struct BaseHold {
    owner: String,
    id: String,
    seq: u64,
    deadline_ns: u64,
    anchor: Vec<f64>,
    wheels_raw: Option<WheelsRaw>,
    closed: bool,
    cancel_sent: bool,
}

pub(super) struct RpcContext<'a> {
    pub client: &'a mut RobotControlClient<Channel>,
    pub token: &'a str,
    pub timeout: Duration,
    pub shutdown: &'a mut tokio::sync::watch::Receiver<bool>,
    pub commands: Option<&'a command::Mirrors>,
    pub clock: &'a dyn RobotClock,
}

pub(super) struct ControlState {
    enabled: bool,
    process_id: String,
    revision: u64,
    task: Option<TaskView>,
    last_request: Option<Request>,
    last_result: Option<Reply>,
    after_stamp_ns: Option<u64>,
    base: Option<BaseHold>,
    base_sequence: u64,
}

impl ControlState {
    pub(super) fn new(enabled: bool, process_id: &str) -> Self {
        Self {
            enabled,
            process_id: process_id.into(),
            revision: 0,
            task: None,
            last_request: None,
            last_result: None,
            after_stamp_ns: None,
            base: None,
            base_sequence: 0,
        }
    }

    pub(super) fn view(&self, motion: Option<&MotionConfig>) -> View {
        View {
            enabled: self.enabled && motion.is_some(),
            can_enable: false,
            enable_reason: if self.task.as_ref().is_some_and(TaskView::busy) {
                "总线已有进行中或结果未知的任务"
            } else {
                "等待使能原位对齐配置、完整状态及S1命令镜像"
            }
            .into(),
            process_id: self.process_id.clone(),
            revision: self.revision.to_string(),
            busy: self.task.as_ref().is_some_and(TaskView::busy),
            max_duration_ms: motion.map_or(0, |m| m.max_duration_ms),
            joints: motion.map_or_else(Vec::new, |m| m.joints.clone()),
            task: self.task.clone(),
            last_result: self.last_result.clone(),
            base: BaseView::default(),
        }
    }

    fn preflight(&self, request: &Request, source: &Source, state: &BusView) -> Result<(), String> {
        request.validate_shape()?;
        if request.is_base() {
            return Err("底盘命令必须经过本连接的有界入口".into());
        }
        if !self.enabled || source.motion.is_none() {
            return Err("此总线未开放Web控制，未发送".into());
        }
        if request.process_id != self.process_id || request.revision != self.revision.to_string() {
            return Err("控制版本已变化；请查看最新状态后重新操作，本请求未发送".into());
        }
        if matches!(
            request.operation,
            Operation::Move | Operation::ClearEstop | Operation::Enable
        ) {
            if self.task.as_ref().is_some_and(TaskView::busy) {
                return Err("总线已有进行中或结果未知的任务，未发送".into());
            }
            if state.data_status != "fresh" || state.fault_code != Some(0) {
                return Err("反馈无效、陈旧或存在故障，未发送".into());
            }
        }
        if request.operation == Operation::Enable && source.enable_alignment_tolerance_rad.is_none()
        {
            return Err("未配置使能原位对齐容差，未发送使能".into());
        }
        if request.operation == Operation::Move {
            if self.after_stamp_ns.is_some_and(|after| {
                state
                    .stamp_ns
                    .as_ref()
                    .and_then(|stamp| stamp.parse::<u64>().ok())
                    .is_none_or(|stamp| stamp <= after)
            }) {
                return Err("上次写入后尚无推进的源反馈，定位未发送".into());
            }
            validate_move(request, source, state)?;
        }
        Ok(())
    }

    pub(super) async fn dispatch(
        &mut self,
        request: Request,
        source: &Source,
        state: &BusView,
        mut rpc: RpcContext<'_>,
    ) -> Reply {
        if *rpc.shutdown.borrow() {
            return Reply::rejected(Some(&request), self.revision, "Web正在关闭，未发送");
        }
        // 精确重放只返回上一回执；更早请求因版本不匹配拒绝，不重新派发。
        if self.last_request.as_ref() == Some(&request)
            && let Some(reply) = &self.last_result
        {
            return reply.clone();
        }
        if let Err(error) = self.preflight(&request, source, state) {
            return Reply::rejected(Some(&request), self.revision, error);
        }
        let enable_stamp = match prepare_operation(&request, source, &mut rpc).await {
            Ok(stamp) => stamp,
            Err(error) => return Reply::rejected(Some(&request), self.revision, error),
        };
        let Some(next_revision) = self.revision.checked_add(1) else {
            return Reply::rejected(Some(&request), self.revision, "控制版本耗尽，未发送");
        };
        let Some(motion) = source.motion.as_ref() else {
            return Reply::rejected(Some(&request), self.revision, "缺少运动配置，未发送");
        };
        if request.operation == Operation::Cancel && !self.task.as_ref().is_some_and(TaskView::busy)
        {
            return Reply::rejected(
                Some(&request),
                self.revision,
                "当前没有可取消的共享任务，未发送",
            );
        }
        self.revision = next_revision;
        self.after_stamp_ns = enable_stamp.or(Some(
            state
                .stamp_ns
                .as_ref()
                .and_then(|stamp| stamp.parse().ok())
                .unwrap_or(0),
        ));
        let rpc_id = uuid::Uuid::now_v7().to_string();
        let action_id = match request.operation {
            Operation::Move => Some(rpc_id.clone()),
            Operation::Cancel => self.task.as_ref().map(|task| task.action_id.clone()),
            _ => None,
        };
        if request.operation == Operation::Move {
            self.task = Some(TaskView {
                action_id: rpc_id.clone(),
                state: "unknown",
                progress: None,
                error: Some("请求已派发，等待明确任务状态".into()),
            });
        }
        let meta = command_meta(&request, source, &rpc_id, motion, &rpc);
        self.close_base_for(request.operation);
        let result = tokio::select! {
            _ = rpc.shutdown.changed() => Err("Web关闭时RPC结果未知；不自动重发"),
            result = tokio::time::timeout(rpc.timeout,
                invoke_rpc(rpc.client, &request, source, meta, action_id.as_deref(), rpc.timeout)) =>
                result.map_err(|_| "RPC超时，是否生效未知；不自动重发"),
        };
        let mut reply = Reply::rejected(Some(&request), self.revision, "");
        reply.action_id = action_id;
        match result {
            Ok(Ok(response)) if response.matches(&rpc_id) => {
                reply.status = "accepted";
                reply.message = accepted_message(request.operation).into();
                if request.operation == Operation::Move {
                    self.task = Some(TaskView {
                        action_id: rpc_id,
                        state: "pending",
                        progress: None,
                        error: None,
                    });
                }
            }
            Ok(Err(status)) if definitely_rejected(status.code()) => {
                reply.message = format!("gRPC拒绝 {:?}：{}", status.code(), status.message());
                if request.operation == Operation::Move {
                    self.task = Some(TaskView {
                        action_id: rpc_id,
                        state: "failed",
                        progress: None,
                        error: Some(reply.message.clone()),
                    });
                }
            }
            result => {
                reply.status = "unknown";
                reply.message = match result {
                    Err(message) => message.into(),
                    Ok(Err(error)) => format!("RPC结果未知：{error}；不自动重发"),
                    Ok(Ok(_)) => "RPC回执身份不匹配，结果未知；不自动重发".into(),
                };
            }
        }
        self.last_request = Some(request);
        self.last_result = Some(reply.clone());
        reply
    }

    pub(super) async fn poll_task(
        &mut self,
        client: &mut RobotControlClient<Channel>,
        timeout: Duration,
        shutdown: &mut tokio::sync::watch::Receiver<bool>,
    ) {
        if *shutdown.borrow() {
            return;
        }
        let Some(task) = self.task.as_mut().filter(|task| task.busy()) else {
            return;
        };
        let rpc_id = uuid::Uuid::now_v7().to_string();
        let mut request = tonic::Request::new(TaskHandleRef {
            request_id: rpc_id.clone(),
            action_id: task.action_id.clone(),
        });
        request.set_timeout(timeout);
        let result = tokio::select! {
            _ = shutdown.changed() => return,
            result = tokio::time::timeout(timeout, client.get_task_status(request)) => result,
        };
        match result {
            Ok(Ok(response)) => apply_task_status(
                task,
                response.into_inner(),
                &rpc_id,
                self.base
                    .as_ref()
                    .is_some_and(|base| base.id == task.action_id),
            ),
            Ok(Err(error)) => {
                task.state = "unknown";
                task.error = Some(format!("任务查询失败：{error}"));
            }
            Err(_) => {
                task.state = "unknown";
                task.error = Some("任务查询超时，仍不能确认终态".into());
            }
        }
    }
}

async fn prepare_operation(
    request: &Request,
    source: &Source,
    rpc: &mut RpcContext<'_>,
) -> Result<Option<u64>, String> {
    if request.operation != Operation::Enable {
        return Ok(None);
    }
    let commands = rpc.commands.ok_or("缺少共享S1命令镜像，未发送使能")?;
    command::prepare_enable(
        source,
        commands,
        rpc.client,
        rpc.clock,
        rpc.timeout,
        rpc.shutdown,
    )
    .await
    .map(Some)
}

fn command_meta(
    request: &Request,
    source: &Source,
    rpc_id: &str,
    motion: &MotionConfig,
    rpc: &RpcContext<'_>,
) -> CommandMeta {
    let timeout_ms = if request.operation == Operation::Move {
        u64::from(request.duration_ms.unwrap_or(0)) + u64::from(motion.settle_timeout_ms) + 5_000
    } else {
        u64::try_from(rpc.timeout.as_millis()).unwrap_or(u64::MAX)
    };
    CommandMeta {
        request_id: rpc_id.into(),
        source: "robot-status".into(),
        auth: Some(AuthContext {
            token: rpc.token.into(),
            mtls_fingerprint: String::new(),
        }),
        timeout_ms,
        idempotency_key: format!("web-{rpc_id}"),
        dry_run: false,
        audit_reason: format!("Web显式操作 {:?} / {}", request.operation, source.id),
    }
}

fn validate_move(request: &Request, source: &Source, state: &BusView) -> Result<(), String> {
    if state.control_link.as_deref() != Some("Ok")
        || state.mode.as_deref() != Some("Enabled")
        || state.safety_state.as_deref() != Some("TorqueOn")
        || state.output_mode.as_deref() != Some("Active")
    {
        return Err("单关节定位要求已使能、确认上扭且输出活动；不会自动使能".into());
    }
    let motion = source.motion.as_ref().ok_or("缺少运动配置")?;
    let name = request.joint.as_deref().ok_or("缺少关节名")?;
    let joint = motion
        .joints
        .iter()
        .find(|joint| joint.name == name)
        .ok_or("未知或非位置关节，未发送")?;
    let position = request.target_position_rad.ok_or("缺少目标位置")?;
    let duration = request.duration_ms.ok_or("缺少运动时长")?;
    let current = state
        .joints
        .iter()
        .find(|joint| joint.name == name)
        .and_then(|joint| joint.position_rad)
        .ok_or("缺少所选关节反馈")?;
    if !position.is_finite()
        || position.abs() > f64::from(f32::MAX)
        || !(joint.min_position_rad..=joint.max_position_rad).contains(&position)
        || duration == 0
        || duration > motion.max_duration_ms
        || (position - current).abs() > joint.max_velocity_rad_s * f64::from(duration) / 1000.0
    {
        return Err("目标、时长或所需速率超出原运动配置，未发送".into());
    }
    Ok(())
}

fn definitely_rejected(code: tonic::Code) -> bool {
    matches!(
        code,
        tonic::Code::InvalidArgument
            | tonic::Code::PermissionDenied
            | tonic::Code::Unauthenticated
            | tonic::Code::FailedPrecondition
            | tonic::Code::OutOfRange
            | tonic::Code::Unimplemented
    )
}

fn accepted_message(operation: Operation) -> &'static str {
    match operation {
        Operation::Move => "任务已接收，等待新鲜反馈到位及任务终态",
        Operation::Cancel => "取消请求已接收；取消不等于断扭，等待任务终态",
        Operation::Enable => "使能回执已收到；等待新鲜TorqueOn反馈后再定位",
        Operation::Disable => "断扭回执已收到；仍以新鲜TorqueOffConfirmed反馈确认",
        Operation::Estop => "急停事件已发布；回执不证明实际已停止或锁存写入完成",
        Operation::ClearEstop => "独立解除急停回执已收到；没有自动使能或清故障",
        Operation::BaseHold => "模拟底盘租期已接收；仅新按住消息可续租",
        Operation::BaseStop => "底盘取消请求已接收；归零与物理停止分别确认",
    }
}

fn apply_task_status(task: &mut TaskView, status: TaskStatus, rpc_id: &str, base_task: bool) {
    if status.request_id != rpc_id
        || status.action_id != task.action_id
        || !status.progress.is_finite()
        || !(0.0..=1.0).contains(&status.progress)
    {
        task.state = "unknown";
        task.error = Some("任务回执身份、进度或状态无效".into());
        return;
    }
    task.progress = Some(status.progress);
    task.error = None;
    task.state = match TaskState::try_from(status.state) {
        Ok(TaskState::Pending) => "pending",
        Ok(TaskState::Executing) => "executing",
        Ok(TaskState::Completed)
            if status
                .outcome
                .as_ref()
                .is_some_and(|outcome| outcome.kind == 1 || (base_task && outcome.kind == 2)) =>
        {
            "completed"
        }
        Ok(TaskState::Failed)
            if status
                .failure
                .as_ref()
                .is_some_and(|failure| (1..=5).contains(&failure.kind)) =>
        {
            task.error = status
                .failure
                .map(|failure| format!("任务失败 {}：{}", failure.kind, failure.reason));
            "failed"
        }
        Ok(TaskState::Cancelled) => "cancelled",
        _ => {
            task.error = Some("任务终态或结局无法确认".into());
            "unknown"
        }
    };
}

enum RpcReply {
    Task(TaskHandle),
    Ack(Ack),
}
impl RpcReply {
    fn matches(&self, id: &str) -> bool {
        match self {
            Self::Task(task) => task.request_id == id && task.action_id == id,
            Self::Ack(ack) => ack.request_id == id && !ack.dry_run,
        }
    }
}

async fn invoke_rpc(
    client: &mut RobotControlClient<Channel>,
    request: &Request,
    source: &Source,
    meta: CommandMeta,
    action_id: Option<&str>,
    timeout: Duration,
) -> Result<RpcReply, tonic::Status> {
    fn bounded<T>(body: T, timeout: Duration) -> tonic::Request<T> {
        let mut request = tonic::Request::new(body);
        request.set_timeout(timeout);
        request
    }
    match request.operation {
        Operation::Move => {
            let mut builder = FlatBufferBuilder::with_capacity(256);
            let hardware = builder.create_string(&source.hardware_id);
            let joint = builder.create_string(request.joint.as_deref().unwrap_or_default());
            let payload = robot_msg::msg::SingleJointPositionPayload::create(
                &mut builder,
                &robot_msg::msg::SingleJointPositionPayloadArgs {
                    hardware_id: Some(hardware),
                    joint_name: Some(joint),
                    target_position_rad: request.target_position_rad.unwrap_or_default(),
                    duration_ms: request.duration_ms.unwrap_or_default(),
                },
            );
            builder.finish(payload, None);
            client
                .invoke_task(bounded(
                    InvokeTaskCommand {
                        meta: Some(meta),
                        kind: ActionKindProto::ActionKindScripted as i32,
                        deadline_ns: 0,
                        priority: 10,
                        payload_schema_id:
                            robot_msg::schema_id::SCHEMA_SINGLE_JOINT_POSITION_PAYLOAD,
                        payload: builder.finished_data().to_vec(),
                    },
                    timeout,
                ))
                .await
                .map(|reply| RpcReply::Task(reply.into_inner()))
        }
        Operation::Cancel => client
            .cancel_task(bounded(
                CancelTaskCommand {
                    meta: Some(meta),
                    action_id: action_id.unwrap_or_default().into(),
                },
                timeout,
            ))
            .await
            .map(|reply| RpcReply::Ack(reply.into_inner())),
        Operation::Disable | Operation::Enable => client
            .set_enabled(bounded(
                SetEnabledCommand {
                    meta: Some(meta),
                    enabled: request.operation == Operation::Enable,
                },
                timeout,
            ))
            .await
            .map(|reply| RpcReply::Ack(reply.into_inner())),
        Operation::Estop => client
            .e_stop(bounded(
                EStopCommand {
                    meta: Some(meta),
                    estop_reason: "Web显式急停".into(),
                },
                timeout,
            ))
            .await
            .map(|reply| RpcReply::Ack(reply.into_inner())),
        Operation::ClearEstop => client
            .clear_e_stop(bounded(
                ClearEStopCommand {
                    meta: Some(meta),
                    confirmation: "CONFIRM_CLEAR_ESTOP".into(),
                },
                timeout,
            ))
            .await
            .map(|reply| RpcReply::Ack(reply.into_inner())),
        Operation::BaseHold | Operation::BaseStop => Err(tonic::Status::failed_precondition(
            "底盘命令必须经过有界连接入口",
        )),
    }
}

#[cfg(test)]
#[path = "status_control_tests.rs"]
mod tests;
