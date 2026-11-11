//! 附着已有默认 S0，复用受监督生命周期执行右侧混合发送或左臂与头部原位保持。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use iceoryx2::prelude::NodeBuilder;
use iceoryx2::service::ipc::Service;
use robot_core::adapter::JointSelection;
use robot_core::time::{RobotClock, SystemClock};
use robot_msg::{build_helpers, markers, msg};
use robot_rpc::client::layout::{HardwareLayout, load_hardware_layout};
use robot_s0::config_loader::{
    load_robot_config, resolve_hardware_section, validate_hardware_section,
};
use robot_s0::grouped_position::{XleRobotBus1Layout, XleRobotBus2Layout};
use robot_transport::iceoryx2::request_response::{Iceoryx2PendingResponse, Iceoryx2RrClient};
use robot_transport::iceoryx2::{
    Iceoryx2Publisher, Iceoryx2Subscriber, RrProfile, StrictChannelStatus, TopicClass, publisher,
    rr_client_with, subscriber,
};
use robot_transport::traits::{LatestSubscriber, Publisher};
use serde_json::{Value, json};

#[path = "supervised_mixed/observation.rs"]
mod observation;
use observation::{MotionSample, Observations, ZeroSample, validate_zero_sample};

const FRESH_NS: u64 = 200_000_000;
const HEARTBEAT_NS: u64 = 20_000_000;
const PERIOD_NS: u64 = 50_000_000;
const PULSE_NS: u64 = 20_000_000_000;
const ZERO_NS: u64 = 500_000_000;
const ENABLE_NS: u64 = 300_000_000;
const DISABLE_NS: u64 = 200_000_000;
const TOTAL_NS: u64 = 21_000_000_000;
const PREPARE_NS: u64 = 5_000_000_000;
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

const HELP: &str = "爸爸，默认 S0 固定前轮发送模式（尚未接通外层维护PLAN/GO/锁）：
supervised_demo --s0-mixed-hold --config /绝对路径/bus2.toml --hardware-id ID \
  --s0-log /绝对路径/s0.json.log --output /绝对路径/全新结果.json \
  --go-expires-at-unix-ns UNIX纳秒 --ack-safety-checklist
仅附着已有默认 S0，不启动服务、不打开UART；须持续看护并具备立即断电条件。
只验证发送、原始PP/PV、零速原字和断扭；物理观察及外层STOP51尚未验收。
左臂与头部改用 --s0-position-hold：仅接受无sensor的左总线八关节配置，
冻结新鲜反馈原位保持20秒；不需要 --s0-log，不执行往返或轮速度发送。";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Mixed,
    PositionHold,
}

#[derive(Debug)]
struct Args {
    mode: Mode,
    config: PathBuf,
    hardware_id: String,
    log: PathBuf,
    output: PathBuf,
    expires_unix_ns: u64,
}

impl Args {
    fn parse(mut raw: impl Iterator<Item = String>, mode: Mode) -> Result<Option<Self>, String> {
        let mut config = None;
        let mut hardware_id = None;
        let mut log = None;
        let mut output = None;
        let mut expires = None;
        let mut ack = false;
        while let Some(arg) = raw.next() {
            if arg == "--help" {
                return Ok(None);
            }
            if arg == "--ack-safety-checklist" {
                if ack {
                    return Err("安全确认参数重复".into());
                }
                ack = true;
                continue;
            }
            let value = raw.next().ok_or_else(|| format!("{arg} 缺少值"))?;
            let slot = match arg.as_str() {
                "--config" => &mut config,
                "--hardware-id" => &mut hardware_id,
                "--s0-log" => &mut log,
                "--output" => &mut output,
                "--go-expires-at-unix-ns" => &mut expires,
                _ => return Err(format!("未知参数：{arg}")),
            };
            if slot.replace(value).is_some() {
                return Err(format!("重复参数：{arg}"));
            }
        }
        if !ack {
            return Err(match mode {
                Mode::Mixed => "必须确认支撑、卸载、三轮稳定架空、区域清空、持续看护及立即断电能力；缺少 --ack-safety-checklist",
                Mode::PositionHold => "必须确认左臂与头部支撑、卸载、区域清空、持续看护及立即断电能力；缺少 --ack-safety-checklist",
            }.into());
        }
        let args = Self {
            mode,
            config: PathBuf::from(config.ok_or("缺少 --config")?),
            hardware_id: hardware_id.ok_or("缺少 --hardware-id")?,
            log: match (log, mode) {
                (Some(path), _) => PathBuf::from(path),
                (None, Mode::Mixed) => return Err("缺少 --s0-log".into()),
                (None, Mode::PositionHold) => PathBuf::new(),
            },
            output: PathBuf::from(output.ok_or("缺少 --output")?),
            expires_unix_ns: expires
                .ok_or("缺少 --go-expires-at-unix-ns")?
                .parse()
                .map_err(|_| "GO到期必须为u64纳秒")?,
        };
        if !args.config.is_absolute()
            || !args.output.is_absolute()
            || (!args.log.as_os_str().is_empty() && !args.log.is_absolute())
        {
            return Err("配置、日志及输出必须为绝对路径".into());
        }
        if args.hardware_id.is_empty() || args.output.exists() {
            return Err("硬件ID不能为空，输出必须全新".into());
        }
        check_go(args.expires_unix_ns)?;
        Ok(Some(args))
    }
}

fn check_go(expires: u64) -> Result<(), String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    if now >= u128::from(expires) {
        return Err("本轮GO已到期；不得续期或继续非零发送".into());
    }
    Ok(())
}

fn add(now: u64, delta: u64) -> Result<u64, String> {
    now.checked_add(delta).ok_or_else(|| "绝对时间溢出".into())
}

fn before(now: u64, deadline: u64) -> Result<(), String> {
    if now >= deadline {
        Err(format!("原绝对期限已过：now={now}, deadline={deadline}"))
    } else {
        Ok(())
    }
}

struct Config {
    mode: Mode,
    robot: String,
    hardware: String,
    layout: HardwareLayout,
    arm_slots: [usize; 6],
    wheel_slots: [usize; 3],
    wheel_ids: [u8; 3],
    selected: usize,
    position_ranges: Option<[(i32, i32); 8]>,
}

impl Config {
    fn load(args: &Args) -> Result<Self, String> {
        let loaded = load_robot_config(&args.config).map_err(|e| e.to_string())?;
        let hardware = resolve_hardware_section(&loaded.parsed, &args.hardware_id)
            .ok_or("硬件ID不在配置内")?;
        validate_hardware_section(hardware).map_err(|e| e.to_string())?;
        if args.mode == Mode::PositionHold {
            if hardware.auto_enable != Some(false)
                || hardware.feetech_bus2_mixed_raw
                || !loaded.parsed.sensor.is_empty()
            {
                return Err("原位保持只接受auto_enable=false、无sensor的左总线Position配置".into());
            }
            XleRobotBus1Layout::from_hardware(hardware).map_err(|e| e.to_string())?;
            let ranges = Self::load_position_ranges(hardware)?;
            let layout =
                load_hardware_layout(&args.config, &args.hardware_id).map_err(|e| e.to_string())?;
            if std::fs::read(&args.config).map_err(|e| e.to_string())? != loaded.raw_bytes {
                return Err("读取过程中配置改变".into());
            }
            return Ok(Self {
                mode: args.mode,
                robot: loaded.parsed.robot.id,
                hardware: args.hardware_id.clone(),
                layout,
                arm_slots: [0; 6],
                wheel_slots: [0; 3],
                wheel_ids: [0; 3],
                selected: 0,
                position_ranges: Some(ranges),
            });
        }
        if !hardware.feetech_bus2_mixed_raw || hardware.auto_enable != Some(false) {
            return Err("只接受显式raw混合且auto_enable=false的默认S0配置".into());
        }
        let mixed = XleRobotBus2Layout::from_hardware(hardware).map_err(|e| e.to_string())?;
        let command = mixed
            .command([0.0; 6], [0; 3], 1, 0, 1)
            .map_err(|e| e.to_string())?;
        let JointSelection::MixedPositionRawVelocity {
            position_mask,
            velocity_mask,
        } = command.selection
        else {
            return Err("配置未生成混合布局".into());
        };
        let arm_slots: Vec<_> = (0..9)
            .filter(|slot| position_mask & (1 << slot) != 0)
            .collect();
        let wheel_slots: Vec<_> = (0..9)
            .filter(|slot| velocity_mask & (1 << slot) != 0)
            .collect();
        let arm_slots = arm_slots.try_into().map_err(|_| "六臂槽位数量错误")?;
        let wheel_slots: [usize; 3] = wheel_slots.try_into().map_err(|_| "三轮槽位数量错误")?;
        let selected = hardware
            .joints
            .iter()
            .position(|joint| joint == "base_front_wheel")
            .ok_or("缺少配置前轮")?;
        let layout =
            load_hardware_layout(&args.config, &args.hardware_id).map_err(|e| e.to_string())?;
        if std::fs::read(&args.config).map_err(|e| e.to_string())? != loaded.raw_bytes {
            return Err("读取过程中配置改变".into());
        }
        let wheel_ids = wheel_slots.map(|slot| hardware.motor_ids[slot]);
        Ok(Self {
            mode: args.mode,
            robot: loaded.parsed.robot.id,
            hardware: args.hardware_id.clone(),
            wheel_ids,
            layout,
            arm_slots,
            wheel_slots,
            selected,
            position_ranges: None,
        })
    }

    fn load_position_ranges(
        hardware: &robot_s0::config_loader::HardwareSection,
    ) -> Result<[(i32, i32); 8], String> {
        let paths = hardware
            .extras
            .get("calibration_paths")
            .and_then(toml::Value::as_array)
            .ok_or("左总线缺少原校准路径")?
            .iter()
            .map(|path| path.as_str().ok_or("校准路径必须为字符串"))
            .collect::<Result<Vec<_>, _>>()?;
        if paths.is_empty() {
            return Err("左总线必须使用原校准，不能无边界发送".into());
        }
        let calibrations =
            robot_adapter_feetech::load_and_merge(&paths).map_err(|e| e.to_string())?;
        let mut ranges = Vec::with_capacity(8);
        for (slot, (name, id)) in hardware.joints.iter().zip(&hardware.motor_ids).enumerate() {
            let calibration = calibrations
                .get(name)
                .ok_or_else(|| format!("缺少原校准：{name}"))?;
            if calibration.id != *id || calibration.range_min > calibration.range_max {
                return Err(format!("原校准ID或上下界不符：{name}"));
            }
            let tolerance = hardware
                .calibration_range_tolerances_raw
                .as_ref()
                .map_or(0, |values| values[slot]);
            ranges.push((
                i32::from(calibration.range_min.saturating_sub(tolerance)),
                i32::from(calibration.range_max.saturating_add(tolerance).min(4095)),
            ));
        }
        ranges.try_into().map_err(|_| "左总线校准必须八项".into())
    }

    fn position_slots(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.layout.motor_ids.len()).filter(|slot| match self.mode {
            Mode::Mixed => self.arm_slots.contains(slot),
            Mode::PositionHold => true,
        })
    }

    fn check_position_target(&self, target: &[f64; 9]) -> Result<(), String> {
        if let Some(ranges) = &self.position_ranges {
            for (slot, &(min, max)) in ranges.iter().enumerate() {
                let position = target[slot];
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "实际Position适配器将f64反馈目标转换成f32；发送前复核同一原字"
                )]
                let raw = robot_adapter_feetech::rad_to_raw(position as f32);
                if !position.is_finite()
                    || position.abs() > f64::from(f32::MAX)
                    || !(min..=max).contains(&raw)
                {
                    return Err(format!(
                        "原位目标越过原校准与容差边界，禁止publish/Enable：slot={slot}, id={}, raw={raw}, range={min}..={max}",
                        self.layout.motor_ids[slot]
                    ));
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct State {
    stamp: u64,
    positions: [f64; 9],
    raw: [i32; 9],
    mode: msg::S0Mode,
    link: msg::S0ControlLink,
    output: msg::S0OutputMode,
    safety: msg::S0SafetyState,
}

impl State {
    #[allow(
        clippy::cast_possible_truncation,
        reason = "S0反馈原始为f32；复用Feetech原换算并校验12位范围"
    )]
    fn decode(value: &msg::JointState<'_>, config: &Config, now: u64) -> Result<Self, String> {
        if value.layout_id() != config.layout.layout_id {
            return Err("JointState layout不匹配".into());
        }
        let positions = value.position_rad().ok_or("缺少位置向量")?;
        if positions.len() != config.layout.motor_ids.len() {
            return Err("JointState必须完整匹配配置槽位".into());
        }
        let positions: [f64; 9] = std::array::from_fn(|slot| {
            if slot < positions.len() {
                positions.get(slot)
            } else {
                0.0
            }
        });
        let stamp = value.stamp_ns();
        if now.checked_sub(stamp).is_none_or(|age| age > FRESH_NS) {
            return Err("JointState来自未来或超过200ms".into());
        }
        if positions
            .iter()
            .any(|v| !v.is_finite() || v.abs() > f64::from(f32::MAX))
        {
            return Err("JointState位置非有限或不可表示".into());
        }
        let raw = positions.map(|position| robot_adapter_feetech::rad_to_raw(position as f32));
        if raw.iter().any(|value| !(0..=4095).contains(value)) {
            return Err("JointState PP不在12位范围".into());
        }
        Ok(Self {
            stamp,
            positions,
            raw,
            mode: value.s0_mode(),
            link: value.s0_control_link(),
            output: value.s0_output_mode(),
            safety: value.s0_safety_state(),
        })
    }
    fn torque_on(&self) -> bool {
        self.mode == msg::S0Mode::Enabled
            && self.link == msg::S0ControlLink::Ok
            && self.output == msg::S0OutputMode::Active
            && self.safety == msg::S0SafetyState::TorqueOn
    }
    fn torque_off(&self) -> bool {
        self.mode == msg::S0Mode::Safe && self.safety == msg::S0SafetyState::TorqueOffConfirmed
    }
}

#[derive(Clone)]
struct Anchor {
    stamp: u64,
    target: [f64; 9],
    raw: [i32; 9],
}

struct MotionCheck {
    start: u64,
    next_window: u64,
    previous: Option<i32>,
    last_started: Option<u64>,
    positive_velocity: bool,
    travel: i64,
    samples: Vec<Value>,
}

impl MotionCheck {
    fn new(start: u64) -> Self {
        Self {
            start,
            next_window: 1,
            previous: None,
            last_started: None,
            positive_velocity: false,
            travel: 0,
            samples: Vec::with_capacity(79),
        }
    }

    fn accept(
        &mut self,
        sample: &MotionSample,
        config: &Config,
        anchor: &Anchor,
    ) -> Result<(), String> {
        if self.next_window >= 80 {
            return Ok(());
        }
        let lower = add(self.start, self.next_window * 5 * PERIOD_NS)?;
        let upper = add(lower, 5 * PERIOD_NS)?;
        if sample.started_ns < lower {
            return Ok(());
        }
        if sample.started_ns >= upper || sample.finished_ns > upper {
            return Err(format!("运动第{}个原250ms窗口缺少完整组", self.next_window));
        }
        if self
            .last_started
            .is_some_and(|last| sample.started_ns <= last)
        {
            return Err("运动观测组重复或时间未推进".into());
        }
        let mut selected = None;
        for wheel in sample.wheels {
            let slot = config
                .layout
                .motor_ids
                .iter()
                .position(|id| *id == wheel.id)
                .ok_or("运动组电机不属于配置")?;
            if wheel.position > 4095 {
                return Err("运动组PP不在12位范围".into());
            }
            if slot == config.selected {
                if wheel.velocity > 1000 {
                    return Err("前轮PV原字超过1000或方向反向".into());
                }
                selected = Some(i32::from(wheel.position));
                self.positive_velocity |= wheel.velocity > 0;
            } else if !matches!(wheel.velocity, 0 | 50 | 0x8032)
                || (i32::from(wheel.position) - anchor.raw[slot]).abs() > 4
            {
                return Err("未选轮PP/PV超出原非目标范围".into());
            }
        }
        let position = selected.ok_or("运动组缺少前轮")?;
        if let Some(last) = self.previous {
            self.travel += i64::from(forward_step(last, position)?);
        }
        self.previous = Some(position);
        self.last_started = Some(sample.started_ns);
        self.samples.push(json!({"window":self.next_window,"started_ns":sample.started_ns,"finished_ns":sample.finished_ns,
            "wheels":sample.wheels.map(|wheel|json!({"id":wheel.id,"position":wheel.position,"velocity":wheel.velocity}))}));
        self.next_window += 1;
        Ok(())
    }

    fn check_deadline(&self, now: u64) -> Result<(), String> {
        if self.next_window < 80 && now >= add(self.start, (self.next_window + 1) * 5 * PERIOD_NS)?
        {
            return Err(format!("运动第{}个250ms窗口已过期", self.next_window));
        }
        Ok(())
    }

    fn finish(&self) -> Result<(), String> {
        if self.samples.len() != 79 || self.travel <= 0 || !self.positive_velocity {
            return Err("缺少79组运动原字、正PV及正累计PP；发送成功不证明动作".into());
        }
        Ok(())
    }
}
impl Anchor {
    fn freeze(state: &State, config: &Config) -> Self {
        let mut target = [0.0; 9];
        for slot in config.position_slots() {
            target[slot] = state.positions[slot];
        }
        Self {
            stamp: state.stamp,
            target,
            raw: state.raw,
        }
    }
    fn check(&self, state: &State, config: &Config) -> Result<(), String> {
        for slot in config.position_slots().chain(
            config
                .wheel_slots
                .into_iter()
                .filter(|slot| config.mode == Mode::Mixed && *slot != config.selected),
        ) {
            if (state.raw[slot] - self.raw[slot]).abs() > 4 {
                return Err(format!("固定锚点偏差超过4raw：slot={slot}"));
            }
        }
        Ok(())
    }
}

struct Reply<T: markers::BodyMarker> {
    pending: Iceoryx2PendingResponse<T>,
    id: String,
    request: Vec<u8>,
    response: Option<Vec<u8>>,
    count: u8,
    closed: bool,
}
impl<T: markers::BodyMarker> Reply<T> {
    fn receive(&mut self) -> Result<bool, String> {
        for _ in 0..2 {
            if let Some(bytes) = self.pending.try_recv().map_err(|e| e.to_string())? {
                self.count = self.count.saturating_add(1);
                if self.response.is_none() {
                    self.response = Some(bytes);
                }
                if self.count != 1 {
                    return Err(format!("{} 收到重复RR响应", self.id));
                }
            }
            if self.pending.connection_state() != StrictChannelStatus::Closed {
                break;
            }
            self.closed = true;
        }
        if let Some(issue) = self.pending.delivery_issue() {
            return Err(format!("{} 已入队但投递异常：{issue:?}", self.id));
        }
        if self.closed && self.count != 1 {
            return Err(format!("{} RR退休但缺少唯一响应", self.id));
        }
        Ok(self.closed)
    }
    fn evidence(&self) -> Value {
        json!({"request_id":self.id,"request_wire":self.request,"response_wire":self.response,"reply_count":self.count,"closed":self.closed})
    }
}

fn send<Req: markers::BodyMarker, Resp: markers::BodyMarker>(
    client: &Iceoryx2RrClient<Req, Resp>,
    id: String,
    request: Vec<u8>,
) -> Result<Reply<Resp>, String> {
    let pending = client.send_typed(&request).map_err(|e| format!("{e:?}"))?;
    Ok(Reply {
        pending,
        id,
        request,
        response: None,
        count: 0,
        closed: false,
    })
}

fn check_set(bytes: &[u8], id: &str, dry: bool) -> Result<msg::SetEnabledStatus, String> {
    let reply = flatbuffers::root::<msg::SetEnabledResponse>(bytes).map_err(|e| e.to_string())?;
    if reply.request_id() != Some(id) || !reply.retired() {
        return Err("SetEnabled响应身份或退休字段异常".into());
    }
    if dry {
        match (reply.status(), reply.domain(), reply.effect()) {
            (msg::RpcStatus::Ok, msg::SetEnabledStatus::WouldAccept, msg::SideEffect::None)
            | (
                msg::RpcStatus::FailedPrecondition,
                msg::SetEnabledStatus::LatchBlocked | msg::SetEnabledStatus::GuardRejected,
                msg::SideEffect::None,
            ) => Ok(reply.domain()),
            _ => Err(format!("Enable只读评估不符：{reply:?}")),
        }
    } else if reply.status() == msg::RpcStatus::Ok
        && reply.domain() == msg::SetEnabledStatus::Ok
        && reply.effect() == msg::SideEffect::Committed
        && reply.completed_at_ns() != 0
        && reply.error_msg().is_none_or(str::is_empty)
    {
        Ok(reply.domain())
    } else {
        Err(format!("SetEnabled未取得成功逻辑提交：{reply:?}"))
    }
}

fn check_clear(bytes: &[u8], id: &str) -> Result<(), String> {
    let reply = flatbuffers::root::<msg::ClearEStopResponse>(bytes).map_err(|e| e.to_string())?;
    if reply.request_id() == Some(id)
        && reply.status() == msg::RpcStatus::Ok
        && reply.domain() == msg::ClearEStopStatus::Cleared
        && reply.effect() == msg::SideEffect::Committed
        && reply.retired()
        && !reply.dry_run()
        && !reply.superseded()
        && reply.cleared_at_ns() != 0
        && reply.error_msg().is_none_or(str::is_empty)
    {
        Ok(())
    } else {
        Err(format!("ClearEStop未取得唯一成功退休：{reply:?}"))
    }
}

#[derive(Default)]
struct Controls {
    probes: Vec<Reply<markers::SetEnabledResponse>>,
    clear: Option<Reply<markers::ClearEStopResponse>>,
    enable: Option<Reply<markers::SetEnabledResponse>>,
    disable: Option<Reply<markers::SetEnabledResponse>>,
}
impl Controls {
    fn evidence(&self) -> Value {
        json!({"probes":self.probes.iter().map(Reply::evidence).collect::<Vec<_>>(),"clear":self.clear.as_ref().map(Reply::evidence),"enable":self.enable.as_ref().map(Reply::evidence),"disable":self.disable.as_ref().map(Reply::evidence)})
    }

    fn drain_cleanup(&mut self, errors: &mut Vec<String>) {
        fn drain<T: markers::BodyMarker>(reply: &mut Reply<T>, errors: &mut Vec<String>) {
            if let Err(error) = reply.receive()
                && !errors.contains(&error)
            {
                errors.push(error);
            }
        }
        for reply in &mut self.probes {
            drain(reply, errors);
        }
        if let Some(reply) = &mut self.clear {
            drain(reply, errors);
        }
        if let Some(reply) = &mut self.enable {
            drain(reply, errors);
        }
        if let Some(reply) = &mut self.disable {
            drain(reply, errors);
        }
    }
}

struct Session {
    config: Config,
    commands: Iceoryx2Publisher<markers::JointCommand>,
    heartbeat: Iceoryx2Publisher<markers::Heartbeat>,
    estop: Iceoryx2Publisher<markers::EStopEvent>,
    state: Iceoryx2Subscriber<markers::JointState>,
    clear_client: Iceoryx2RrClient<markers::ClearEStopRequest, markers::ClearEStopResponse>,
    enable_client: Iceoryx2RrClient<markers::SetEnabledRequest, markers::SetEnabledResponse>,
    latest: Option<State>,
    anchor: Option<Anchor>,
    sequence: u64,
    last_heartbeat: Option<u64>,
    last_zero: Option<u64>,
    enable_attempt: Option<u64>,
    enable_deadline: Option<u64>,
    expires: u64,
    observations: Option<Observations>,
    motion: Option<MotionCheck>,
    _node: iceoryx2::node::Node<Service>,
}

fn publish<T: markers::BodyMarker>(
    output: &mut Iceoryx2Publisher<T>,
    body: &[u8],
    seq: u64,
    stamp: u64,
) -> Result<(), String> {
    let mut sample = output.loan(1024 + body.len()).map_err(|e| e.to_string())?;
    sample.set_seq(seq);
    sample.set_source_id(2);
    sample.set_stamp_ns(stamp);
    sample.set_body_bytes(body);
    sample.publish().map_err(|e| e.to_string())
}

impl Session {
    fn open(
        config: Config,
        expires: u64,
        observations: Option<Observations>,
    ) -> Result<Self, String> {
        let node = NodeBuilder::new()
            .create::<Service>()
            .map_err(|e| format!("{e:?}"))?;
        let prefix = format!("/{}", config.robot);
        let commands = publisher(
            &node,
            &format!("{prefix}/s1/{}/command", config.hardware),
            TopicClass::S0Command,
        )
        .map_err(|e| e.to_string())?;
        let heartbeat = publisher(
            &node,
            &format!("{prefix}/s1/heartbeat"),
            TopicClass::Heartbeat,
        )
        .map_err(|e| e.to_string())?;
        let estop = publisher(
            &node,
            &format!("{prefix}/events/estop"),
            TopicClass::SafetyEvent,
        )
        .map_err(|e| e.to_string())?;
        commands.update_connections().map_err(|e| e.to_string())?;
        heartbeat.update_connections().map_err(|e| e.to_string())?;
        estop.update_connections().map_err(|e| e.to_string())?;
        let state = subscriber(
            &node,
            &format!("{prefix}/s0/{}/state", config.hardware),
            TopicClass::S0State,
        )
        .map_err(|e| e.to_string())?;
        let clear_client = rr_client_with(
            &node,
            &format!("{prefix}/control/clear_estop"),
            &RrProfile::S0_CONTROL,
        )
        .map_err(|e| e.to_string())?;
        let enable_client = rr_client_with(
            &node,
            &format!("{prefix}/control/set_enabled"),
            &RrProfile::S0_CONTROL,
        )
        .map_err(|e| e.to_string())?;
        Ok(Self {
            config,
            commands,
            heartbeat,
            estop,
            state,
            clear_client,
            enable_client,
            latest: None,
            anchor: None,
            sequence: 0,
            last_heartbeat: None,
            last_zero: None,
            enable_attempt: None,
            enable_deadline: None,
            expires,
            observations,
            motion: None,
            _node: node,
        })
    }
    fn id(&mut self, label: &str) -> String {
        self.sequence += 1;
        format!("mixed-{}-{}-{label}", std::process::id(), self.sequence)
    }
    fn guard(&self, now: u64, deadline: u64) -> Result<(), String> {
        if INTERRUPTED.load(Ordering::Acquire) {
            return Err("收到SIGINT/SIGTERM".into());
        }
        check_go(self.expires)?;
        before(now, deadline)?;
        if let Some(end) = self.enable_deadline {
            before(now, end)?;
        }
        Ok(())
    }
    fn refresh_state(&mut self) -> Result<(), String> {
        if let Some(lease) = self.state.latest().map_err(|e| e.to_string())? {
            let view = lease.view().map_err(|e| e.to_string())?;
            let value: msg::JointState = view.body.get();
            let state = State::decode(&value, &self.config, SystemClock.now().0)?;
            if self
                .latest
                .as_ref()
                .is_some_and(|old| state.stamp < old.stamp)
            {
                return Err("状态时间戳倒退".into());
            }
            self.latest = Some(state);
        }
        let now = SystemClock.now().0;
        if self.latest.as_ref().is_some_and(|state| {
            now.checked_sub(state.stamp)
                .is_none_or(|age| age > FRESH_NS)
        }) {
            return Err("状态失去200ms新鲜度".into());
        }
        Ok(())
    }
    fn heartbeat(&mut self, now: u64) -> Result<(), String> {
        if self
            .last_heartbeat
            .is_none_or(|last| now.saturating_sub(last) >= HEARTBEAT_NS)
        {
            self.sequence += 1;
            let body = build_helpers::build_heartbeat(2, self.sequence, 50.0, now);
            publish(&mut self.heartbeat, &body, self.sequence, now)?;
            self.last_heartbeat = Some(now);
        }
        Ok(())
    }
    fn command(&mut self, raw: i16, now: u64, deadline: u64) -> Result<(), String> {
        before(now, deadline)?;
        if self.config.mode == Mode::PositionHold
            && self.latest.as_ref().is_none_or(|state| {
                now.checked_sub(state.stamp)
                    .is_none_or(|age| age >= FRESH_NS)
            })
        {
            return Err("左总线Position发布前缺少200ms内新鲜反馈".into());
        }
        let target = match &self.anchor {
            Some(anchor) => anchor.target,
            None => {
                Anchor::freeze(
                    self.latest.as_ref().ok_or("无新鲜位置，不能发零命令")?,
                    &self.config,
                )
                .target
            }
        };
        self.config.check_position_target(&target)?;
        let mut wheels = [0_i16; 9];
        wheels[self.config.selected] = raw;
        self.sequence += 1;
        let body = match self.config.mode {
            Mode::Mixed => build_helpers::build_mixed_joint_command(
                self.config.layout.layout_id,
                &target,
                &wheels,
                deadline,
                10,
                self.sequence,
            ),
            Mode::PositionHold => {
                if raw != 0 {
                    return Err("左总线原位保持禁止速度目标".into());
                }
                build_helpers::build_joint_command(
                    self.config.layout.layout_id,
                    msg::ControlMode::Position,
                    &target[..8],
                    deadline,
                    10,
                    msg::StaleAction::Hold,
                    self.sequence,
                )
            }
        };
        publish(&mut self.commands, &body, self.sequence, now)
    }
    fn step(&mut self, zero: bool, cleanup: bool) -> Result<Vec<ZeroSample>, String> {
        let now = SystemClock.now().0;
        self.heartbeat(now)?;
        // 左侧必须先刷新再决定能否发布；收尾时即使旧反馈已过期，也要读到新断扭反馈。
        let now = if self.config.mode == Mode::PositionHold {
            self.refresh_state()?;
            SystemClock.now().0
        } else {
            now
        };
        if zero
            && self.latest.is_some()
            && self
                .last_zero
                .is_none_or(|last| now.saturating_sub(last) >= HEARTBEAT_NS)
        {
            let deadline = add(now, FRESH_NS)?;
            self.command(0, now, deadline)?;
            self.last_zero = Some(now);
        }
        if self.config.mode == Mode::Mixed {
            self.refresh_state()?;
        }
        if !cleanup && let (Some(anchor), Some(state)) = (&self.anchor, &self.latest) {
            anchor.check(state, &self.config)?;
        }
        let mut zeros = Vec::new();
        if let Some(observations) = &mut self.observations {
            zeros = observations.poll(&SystemClock)?;
            let motion_samples = observations.take_motion();
            if !cleanup && let (Some(motion), Some(anchor)) = (&mut self.motion, &self.anchor) {
                for sample in motion_samples {
                    motion.accept(&sample, &self.config, anchor)?;
                }
                motion.check_deadline(SystemClock.now().0)?;
            }
        }
        Ok(zeros)
    }
    fn wait_reply<T: markers::BodyMarker>(
        &mut self,
        reply: &mut Reply<T>,
        deadline: u64,
    ) -> Result<Vec<u8>, String> {
        loop {
            self.guard(SystemClock.now().0, deadline)?;
            self.step(true, false)?;
            let done = reply.receive()?;
            self.guard(SystemClock.now().0, deadline)?;
            if done {
                return reply.response.clone().ok_or_else(|| "RR缺少原响应".into());
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    fn set_request(
        &mut self,
        enabled: bool,
        dry: bool,
        deadline: u64,
    ) -> Result<Reply<markers::SetEnabledResponse>, String> {
        if enabled && !dry && self.config.mode == Mode::PositionHold {
            self.config
                .check_position_target(&self.anchor.as_ref().ok_or("缺少启用前原位锚点")?.target)?;
        }
        let id = self.id(if dry {
            "probe"
        } else if enabled {
            "enable"
        } else {
            "disable"
        });
        let now = SystemClock.now().0;
        let body = build_helpers::build_set_enabled_request(
            &id,
            &id,
            match self.config.mode {
                Mode::Mixed => "supervised-s0-mixed",
                Mode::PositionHold => "supervised-s0-position-hold",
            },
            msg::RpcRole::Maintainer,
            enabled,
            dry,
            deadline,
            match self.config.mode {
                Mode::Mixed => "固定前轮受监督发送验收",
                Mode::PositionHold => "左臂与头部受监督原位保持",
            },
            now,
        );
        send(&self.enable_client, id, body)
    }
}

fn prepare(io: &mut Session, controls: &mut Controls) -> Result<(), String> {
    let deadline = add(SystemClock.now().0, PREPARE_NS)?;
    loop {
        io.guard(SystemClock.now().0, deadline)?;
        io.step(true, false)?;
        if let Some(state) = &io.latest {
            if !state.torque_off() {
                return Err("初始状态必须Safe且TorqueOffConfirmed".into());
            }
            // Safe/Ready 的 wire link/output 固定 Unknown；这里只读评估 Ready 前置。
            // live退休与新鲜 Enabled.Active/TorqueOn（含linkOk）仍在启用后独立核验。
            let reply = io.set_request(true, true, deadline)?;
            controls.probes.push(reply);
            let reply = controls.probes.last_mut().ok_or("缺少只读请求")?;
            let bytes = io.wait_reply(reply, deadline)?;
            match check_set(&bytes, &reply.id, true)? {
                msg::SetEnabledStatus::WouldAccept => return Ok(()),
                msg::SetEnabledStatus::LatchBlocked if controls.clear.is_none() => {
                    let id = io.id("clear");
                    let now = SystemClock.now().0;
                    let body = build_helpers::build_clear_estop_request(
                        &id,
                        &id,
                        "supervised-s0-mixed",
                        msg::RpcRole::Maintainer,
                        "现场确认停止且受持续监督",
                        "本轮默认S0验收",
                        false,
                        deadline,
                        now,
                    );
                    controls.clear = Some(send(&io.clear_client, id, body)?);
                    let reply = controls.clear.as_mut().ok_or("缺少Clear原请求")?;
                    let bytes = io.wait_reply(reply, deadline)?;
                    check_clear(&bytes, &reply.id)?;
                }
                msg::SetEnabledStatus::GuardRejected => {}
                _ => return Err("Clear后仍被latch阻止，禁止重试live写".into()),
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn cycle_deadline(start: u64, anchor_stamp: u64, cycle: u64) -> Result<(u64, u64), String> {
    if cycle >= 400 {
        return Err("周期必须在0..400".into());
    }
    let scheduled = add(start, cycle.checked_mul(PERIOD_NS).ok_or("周期乘法溢出")?)?;
    let mut deadline = add(scheduled, PERIOD_NS)?.min(add(start, PULSE_NS)?);
    if cycle == 0 {
        deadline = deadline.min(add(anchor_stamp, FRESH_NS)?);
    }
    Ok((scheduled, deadline))
}

fn pulse_deadlines(attempted: u64, anchor_stamp: u64, start: u64) -> Result<(u64, u64), String> {
    let hard_end = add(attempted, TOTAL_NS)?;
    before(start, add(attempted, ENABLE_NS)?)?;
    before(start, add(anchor_stamp, FRESH_NS)?)?;
    if add(start, PULSE_NS + ZERO_NS + DISABLE_NS)? > hard_end {
        return Err("21秒总预算不足完整20秒+500ms+200ms停止".into());
    }
    Ok((add(start, PULSE_NS)?, hard_end))
}

fn source_deadline(
    attempt: u64,
    enable_end: u64,
    first_anchor: Option<u64>,
) -> Result<u64, String> {
    let mut deadline = add(attempt, FRESH_NS)?.min(enable_end);
    if let Some(stamp) = first_anchor {
        deadline = deadline.min(add(stamp, FRESH_NS)?);
    }
    before(attempt, deadline)?;
    Ok(deadline)
}

fn forward_step(previous: i32, current: i32) -> Result<i32, String> {
    let step = (current - previous).rem_euclid(4096);
    if step > 512 {
        return Err("前轮PP相邻增量超过512或方向反向".into());
    }
    Ok(step)
}

fn zero_slot(sample: &ZeroSample, started: u64, index: usize) -> Result<bool, String> {
    let (lower, upper) = match index {
        0 => (50_000_000, 250_000_000),
        1 => (250_000_000, 450_000_000),
        2 => (450_000_000, ZERO_NS),
        _ => return Ok(false),
    };
    if sample.started_ns < add(started, lower)? {
        return Ok(false);
    }
    if sample.finished_ns > add(started, upper)? {
        return Err(format!("零速第{}组错过原区间", index + 1));
    }
    validate_zero_sample(sample, index == 2)?;
    Ok(true)
}

fn enable_and_start_pulse(
    io: &mut Session,
    controls: &mut Controls,
    report: &mut Value,
) -> Result<(u64, u64, u64), String> {
    prepare(io, controls)?;
    io.refresh_state()?;
    io.anchor = Some(Anchor::freeze(
        io.latest.as_ref().ok_or("缺少启用前锚点")?,
        &io.config,
    ));
    io.config
        .check_position_target(&io.anchor.as_ref().unwrap().target)?;
    let attempted = SystemClock.now().0;
    let hard_end = add(attempted, TOTAL_NS)?;
    io.guard(attempted, add(attempted, ENABLE_NS)?)?;
    io.enable_attempt = Some(attempted);
    io.enable_deadline = Some(hard_end);
    report["enable_attempt_ns"] = json!(attempted);
    report["enable_deadline_ns"] = json!(hard_end);
    if io.config.mode == Mode::PositionHold {
        report["enable_transition_deadline_ns"] = json!(add(attempted, ENABLE_NS)?);
    }
    let count = io.config.layout.motor_ids.len();
    report["anchor"] = json!({"stamp_ns":io.anchor.as_ref().unwrap().stamp,"target":io.anchor.as_ref().unwrap().target[..count],"raw":io.anchor.as_ref().unwrap().raw[..count],"meaning":"发送端冻结反馈目标；不是adapter内部enable GP锚点"});
    controls.enable = Some(io.set_request(true, false, add(attempted, ENABLE_NS)?)?);
    let reply = controls.enable.as_mut().ok_or("缺少Enable原请求")?;
    let bytes = io.wait_reply(reply, add(attempted, ENABLE_NS)?)?;
    check_set(&bytes, &reply.id, false)?;
    loop {
        io.guard(SystemClock.now().0, add(attempted, ENABLE_NS)?)?;
        io.step(true, false)?;
        if io
            .latest
            .as_ref()
            .is_some_and(|state| state.stamp >= attempted && state.torque_on())
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    if io.config.mode == Mode::PositionHold {
        report["enable_confirmed_ns"] = json!(SystemClock.now().0);
    }
    // 首拍前完成所有可变时长轮询；20秒只从紧邻首次发送尝试的时刻开始。
    io.step(false, false)?;
    if !io.latest.as_ref().is_some_and(State::torque_on) {
        return Err("首拍前S0不再Enabled.Active/TorqueOn".into());
    }
    let start = SystemClock.now().0;
    let (pulse_end, _) = pulse_deadlines(attempted, io.anchor.as_ref().unwrap().stamp, start)?;
    let (_, first_deadline) = cycle_deadline(start, io.anchor.as_ref().unwrap().stamp, 0)?;
    io.guard(start, first_deadline)?;
    let source = source_deadline(start, hard_end, Some(io.anchor.as_ref().unwrap().stamp))?;
    io.command(
        match io.config.mode {
            Mode::Mixed => 500,
            Mode::PositionHold => 0,
        },
        start,
        source,
    )?;
    let returned = SystemClock.now().0;
    before(returned, first_deadline)?;
    if io.config.mode == Mode::PositionHold {
        io.refresh_state()?;
        let observed = SystemClock.now().0;
        before(observed, first_deadline)?;
        record_hold_sample(io, report, 0, observed)?;
    }
    match io.config.mode {
        Mode::Mixed => {
            report["nonzero_start_ns"] = json!(start);
            report["nonzero_end_ns"] = json!(pulse_end);
            io.motion = Some(MotionCheck::new(start));
        }
        Mode::PositionHold => {
            report["hold_start_ns"] = json!(start);
            report["hold_end_ns"] = json!(pulse_end);
        }
    }
    report["cycles"]
        .as_array_mut()
        .unwrap()
        .push(json!({"cycle":0,"scheduled_ns":start,"publish_attempt_ns":start,"publish_return_ns":returned,"local_deadline_ns":first_deadline,"source_deadline_ns":source}));
    Ok((start, pulse_end, hard_end))
}

fn hold_sample(
    state: &State,
    anchor: &Anchor,
    config: &Config,
    cycle: u64,
    observed: u64,
    previous_stamp: Option<u64>,
) -> Result<Value, String> {
    if config.mode != Mode::PositionHold || cycle >= 400 || !state.torque_on() {
        return Err("原位保持采样必须为左八槽且Enabled.Active/TorqueOn".into());
    }
    if state.stamp <= anchor.stamp
        || observed
            .checked_sub(state.stamp)
            .is_none_or(|age| age >= FRESH_NS)
        || previous_stamp.is_some_and(|previous| state.stamp <= previous)
    {
        return Err("原位保持反馈超过200ms、来自未来或时间未推进".into());
    }
    anchor.check(state, config)?;
    Ok(json!({"cycle":cycle,"stamp_ns":state.stamp,"observed_ns":observed,"raw":state.raw[..8]}))
}

fn record_hold_sample(
    io: &Session,
    report: &mut Value,
    cycle: u64,
    observed: u64,
) -> Result<(), String> {
    let samples = report["hold_samples"]
        .as_array_mut()
        .ok_or("缺少原位保持采样数组")?;
    let previous_stamp = samples
        .last()
        .and_then(|sample| sample["stamp_ns"].as_u64());
    let sample = hold_sample(
        io.latest.as_ref().ok_or("缺少原位保持反馈")?,
        io.anchor.as_ref().ok_or("缺少冻结原位锚点")?,
        &io.config,
        cycle,
        observed,
        previous_stamp,
    )?;
    samples.push(sample);
    Ok(())
}

fn body(io: &mut Session, controls: &mut Controls, report: &mut Value) -> Result<(), String> {
    let (start, pulse_end, hard_end) = enable_and_start_pulse(io, controls, report)?;
    for cycle in 1..400 {
        let (scheduled, deadline) =
            cycle_deadline(start, io.anchor.as_ref().unwrap().stamp, cycle)?;
        while SystemClock.now().0 < scheduled {
            io.guard(SystemClock.now().0, deadline)?;
            io.step(false, false)?;
            std::thread::sleep(Duration::from_millis(1));
        }
        let now = SystemClock.now().0;
        io.guard(now, deadline)?;
        io.step(false, false)?;
        if !io.latest.as_ref().is_some_and(State::torque_on) {
            return Err("运动中S0不再Enabled.Active/TorqueOn".into());
        }
        let sent = SystemClock.now().0;
        io.guard(sent, deadline)?;
        let source = source_deadline(sent, hard_end, None)?;
        io.command(
            match io.config.mode {
                Mode::Mixed => 500,
                Mode::PositionHold => 0,
            },
            sent,
            source,
        )?;
        let returned = SystemClock.now().0;
        before(returned, deadline)?;
        if io.config.mode == Mode::PositionHold {
            io.refresh_state()?;
            let observed = SystemClock.now().0;
            before(observed, deadline)?;
            record_hold_sample(io, report, cycle, observed)?;
        }
        report["cycles"].as_array_mut().unwrap().push(
            json!({"cycle":cycle,"scheduled_ns":scheduled,"publish_attempt_ns":sent,"publish_return_ns":returned,"local_deadline_ns":deadline,"source_deadline_ns":source}),
        );
    }
    while SystemClock.now().0 < pulse_end {
        io.guard(SystemClock.now().0, pulse_end)?;
        io.step(false, false)?;
        if io.config.mode == Mode::PositionHold && !io.latest.as_ref().is_some_and(State::torque_on)
        {
            return Err("原位保持尾拍S0不再Enabled.Active/TorqueOn".into());
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    match io.config.mode {
        Mode::Mixed => finish_wheel_zero(io, report, hard_end)?,
        Mode::PositionHold => {
            if report["hold_samples"]
                .as_array()
                .is_none_or(|samples| samples.len() != 400)
            {
                return Err("原位保持未取得400拍新鲜反馈".into());
            }
            report["hold_position_verified"] = json!(true);
        }
    }
    disable_and_stop(io, controls, report, hard_end)
}

fn finish_wheel_zero(io: &mut Session, report: &mut Value, hard_end: u64) -> Result<(), String> {
    let zero_start = SystemClock.now().0;
    io.guard(zero_start, hard_end)?;
    let zero_end = add(zero_start, ZERO_NS)?;
    if add(zero_end, DISABLE_NS)? > hard_end {
        return Err("实际零速起点已耗尽原停止余量".into());
    }
    io.command(0, zero_start, add(zero_start, FRESH_NS)?)?;
    report["zero_publish_return_ns"] = json!(SystemClock.now().0);
    io.last_zero = Some(zero_start);
    report["zero_attempt_ns"] = json!(zero_start);
    report["zero_end_ns"] = json!(zero_end);
    let motion = io.motion.as_ref().ok_or("缺少运动原字检查")?;
    motion.finish()?;
    report["motion_position_verified"] = json!(true);
    report["motion_velocity_raw"] = json!("PASS");
    report["position_travel_raw"] = json!(motion.travel);
    let mut accepted = 0;
    while SystemClock.now().0 < zero_end {
        io.guard(SystemClock.now().0, zero_end)?;
        let groups = io.step(true, false)?;
        let observed_at = SystemClock.now().0;
        for sample in groups {
            if observed_at >= zero_end {
                return Err("零速样本在原500ms截止后才接纳".into());
            }
            if report.get("first_gv_zero_device_group").is_none()
                && sample.started_ns >= zero_start
                && sample.wheels.iter().all(|wheel| wheel.goal == 0)
            {
                report["first_gv_zero_device_group"] = json!({"started_ns":sample.started_ns,"finished_ns":sample.finished_ns,"meaning":"设备观测区间，不是ACK或精确切换时刻"});
            }
            if zero_slot(&sample, zero_start, accepted)? {
                report["zero_samples"].as_array_mut().unwrap().push(json!({"started_ns":sample.started_ns,"finished_ns":sample.finished_ns,"wheels":sample.wheels.map(|wheel|json!({"id":wheel.id,"torque":wheel.torque,"goal":wheel.goal,"velocity":wheel.velocity}))}));
                accepted += 1;
                if accepted == 3 {
                    let observations = io.observations.as_ref().ok_or("缺少轮日志观测")?;
                    report["zero_summary_baseline"] = json!(observations.summary_count());
                    report["zero_summary_seen"] = json!(observations.summary_after_last_zero());
                }
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    if accepted != 3 {
        return Err("原500ms内未取得三个分区间完整零速组".into());
    }
    Ok(())
}

fn disable_and_stop(
    io: &mut Session,
    controls: &mut Controls,
    report: &mut Value,
    hard_end: u64,
) -> Result<(), String> {
    let disabled_at = SystemClock.now().0;
    let deadline = add(disabled_at, DISABLE_NS)?.min(hard_end);
    report["disable_attempt_ns"] = json!(disabled_at);
    controls.disable = Some(io.set_request(false, false, deadline)?);
    let reply = controls.disable.as_mut().ok_or("缺少Disable原请求")?;
    let bytes = io.wait_reply(reply, deadline)?;
    check_set(&bytes, &reply.id, false)?;
    report["disable_retired"] = json!(true);
    let confirmed_at = loop {
        io.guard(SystemClock.now().0, deadline)?;
        io.step(true, false)?;
        if io
            .latest
            .as_ref()
            .is_some_and(|state| state.stamp >= disabled_at && state.torque_off())
        {
            let confirmed_at = SystemClock.now().0;
            if io.config.mode == Mode::PositionHold {
                before(confirmed_at, deadline)?;
            }
            break confirmed_at;
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    report["torque_off_confirmed"] = json!(true);
    report["torque_off_confirmed_ns"] = json!(confirmed_at);
    if io.config.mode == Mode::Mixed {
        confirm_dropped_zero(io, report)?;
    }
    io.sequence += 1;
    publish_final_estop(report, &SystemClock, |body, stamp| {
        publish(&mut io.estop, body, io.sequence, stamp)
    })?;
    if io.config.mode == Mode::PositionHold {
        before(SystemClock.now().0, hard_end)?;
    }
    Ok(())
}

fn publish_final_estop(
    report: &mut Value,
    clock: &dyn RobotClock,
    send: impl FnOnce(&[u8], u64) -> Result<(), String>,
) -> Result<(), String> {
    // Disable、实际断扭及日志尾窗已经完成；即使 GO 过期也只收紧，不再启用。
    let started = clock.now().0;
    report["final_estop_attempt_ns"] = json!(started);
    report["final_estop_publish_return_ns"] = Value::Null;
    report["final_estop_published"] = json!(false);
    let body = sender_estop_body("默认S0混合验收完成", started);
    send(&body, started)?;
    report["final_estop_publish_return_ns"] = json!(clock.now().0);
    report["final_estop_published"] = json!(true);
    // Safe/TEoff 和旧关节 stamp 不能确认此次 EStop；外层须等双 latch 后再退出 daemon。
    Ok(())
}

fn sender_estop_body(reason: &str, stamp_ns: u64) -> Vec<u8> {
    // latched 是 S0 已持久化的确认语义；成功与失败发送端都不可知，必须填 false。
    // IPC 接收此事件仍会触发 EStop 收紧与持久化义务，不以该字段授予或阻止动作。
    build_helpers::build_estop_event(reason, stamp_ns, "supervised-s0-mixed", false)
}

fn confirm_dropped_zero(io: &mut Session, report: &mut Value) -> Result<(), String> {
    // 已断扭后的只读日志尾窗，不再发命令或心跳，也不延长21秒启用期限。
    let deadline = add(SystemClock.now().0, 1_200_000_000)?;
    report["readonly_tail_deadline_ns"] = json!(deadline);
    loop {
        if INTERRUPTED.load(Ordering::Acquire) {
            return Err("只读尾窗收到SIGINT/SIGTERM".into());
        }
        let observations = io.observations.as_mut().ok_or("缺少轮日志观测")?;
        observations.poll(&SystemClock)?;
        let _ = observations.take_motion();
        before(SystemClock.now().0, deadline)?;
        if observations.summary_after_last_zero() {
            report["diagnostics_dropped_zero_verified"] = json!(true);
            report["final_summary_count"] = json!(observations.summary_count());
            report["readonly_tail_finished_ns"] = json!(SystemClock.now().0);
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn finalize_error(io: &mut Session, controls: &mut Controls, report: &mut Value) {
    if io.enable_attempt.is_none() {
        return;
    }
    report["prior_torque_off_confirmed"] = report["torque_off_confirmed"].clone();
    report["torque_off_confirmed"] = json!(false);
    let started = SystemClock.now().0;
    let mut errors = Vec::new();
    if let Err(error) = io.command(0, started, started.saturating_add(FRESH_NS)) {
        errors.push(format!("零目标：{error}"));
    }
    let body = sender_estop_body("默认S0混合验收失败或中断", started);
    io.sequence += 1;
    if let Err(error) = publish(&mut io.estop, &body, io.sequence, started) {
        errors.push(format!("EStop：{error}"));
    }
    let deadline = started
        .saturating_add(DISABLE_NS)
        .min(io.enable_deadline.unwrap_or(started));
    loop {
        controls.drain_cleanup(&mut errors);
        if let Err(error) = io.step(true, true)
            && !errors.contains(&error)
        {
            errors.push(error);
        }
        if io.latest.as_ref().is_some_and(|state| {
            state.stamp >= started && state.safety == msg::S0SafetyState::TorqueOffConfirmed
        }) {
            report["torque_off_confirmed"] = json!(true);
            report["torque_off_confirmed_ns"] = json!(SystemClock.now().0);
            break;
        }
        if SystemClock.now().0 >= deadline {
            errors.push("原停止预算内未确认新的TorqueOffConfirmed".into());
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    report["cleanup_errors"] = json!(errors);
    report["manual_power_cut_required"] = json!(report["torque_off_confirmed"] != true);
}

extern "C" fn interrupted(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::Release);
}
struct Signals {
    interrupt: libc::sighandler_t,
    terminate: libc::sighandler_t,
}
impl Signals {
    fn install() -> Result<Self, String> {
        INTERRUPTED.store(false, Ordering::Release);
        // SAFETY：handler仅写无锁原子标志；失败时恢复已安装的handler。
        let interrupt =
            unsafe { libc::signal(libc::SIGINT, interrupted as *const () as libc::sighandler_t) };
        if interrupt == libc::SIG_ERR {
            return Err("SIGINT安装失败".into());
        }
        let terminate = unsafe {
            libc::signal(
                libc::SIGTERM,
                interrupted as *const () as libc::sighandler_t,
            )
        };
        if terminate == libc::SIG_ERR {
            unsafe {
                libc::signal(libc::SIGINT, interrupt);
            }
            return Err("SIGTERM安装失败".into());
        }
        Ok(Self {
            interrupt,
            terminate,
        })
    }
}
impl Drop for Signals {
    fn drop(&mut self) {
        // SAFETY：恢复本入口安装前的两个handler。
        unsafe {
            libc::signal(libc::SIGINT, self.interrupt);
            libc::signal(libc::SIGTERM, self.terminate);
        }
    }
}

fn run(
    args: &Args,
    config: Config,
    mut file: File,
    observer: Option<Observations>,
) -> Result<bool, String> {
    let mut report = initial_report(args, &config);
    let initialized = (|| {
        Ok::<_, String>((
            Signals::install()?,
            Session::open(config, args.expires_unix_ns, observer)?,
        ))
    })();
    let (_signals, mut io) = match initialized {
        Ok(value) => value,
        Err(error) => {
            report["error"] = json!(error);
            return save_report(&mut file, &report);
        }
    };
    let mut controls = Controls::default();
    match body(&mut io, &mut controls, &mut report) {
        Ok(()) => report["result"] = json!("SOFTWARE_PATH_PASS"),
        Err(error) => {
            report["error"] = json!(error);
            finalize_error(&mut io, &mut controls, &mut report);
        }
    }
    report["control_requests"] = controls.evidence();
    report["latest_state"] = json!(io.latest.as_ref().map(|state| json!({
        "stamp_ns": state.stamp,
        "mode": format!("{:?}", state.mode),
        "link": format!("{:?}", state.link),
        "output": format!("{:?}", state.output),
        "safety": format!("{:?}", state.safety),
    })));
    if let Some(motion) = &io.motion {
        report["motion_samples"] = json!(motion.samples);
    }
    report["finished_ns"] = json!(SystemClock.now().0);
    save_report(&mut file, &report)
}

fn initial_report(args: &Args, config: &Config) -> Value {
    let mut report = json!({"result":"FAIL","config":args.config,"hardware_id":config.hardware,"layout_id":config.layout.layout_id,"go_expires_at_unix_ns":args.expires_unix_ns,"outer_maintenance_plan_go_locks_connected":false,"full_hardware_acceptance":false,"deployable":false,"physical_motion":"NOT_OBSERVED","stop51":"NOT_RUN","cycles":[],"disable_retired":false,"torque_off_confirmed":false,"final_estop_attempt_ns":null,"final_estop_publish_return_ns":null,"final_estop_published":false,"cleanup_errors":[],"manual_power_cut_required":false});
    match config.mode {
        Mode::Mixed => {
            report["schema"] = json!("SUPERVISED_S0_MIXED_V1");
            report["selected_motor_id"] = json!(config.layout.motor_ids[config.selected]);
            report["wheel_ids"] = json!(config.wheel_ids);
            report["motion_position_verified"] = json!(false);
            report["motion_velocity_raw"] = json!("NOT_EVALUATED");
            report["motion_samples"] = json!([]);
            report["zero_samples"] = json!([]);
        }
        Mode::PositionHold => {
            report["schema"] = json!("SUPERVISED_S0_POSITION_HOLD_V1");
            report["motor_ids"] = json!(config.layout.motor_ids);
            report["hold_start_ns"] = Value::Null;
            report["hold_end_ns"] = Value::Null;
            report["hold_position_verified"] = json!(false);
            report["hold_samples"] = json!([]);
            report["enable_attempt_ns"] = Value::Null;
            report["enable_deadline_ns"] = Value::Null;
            report["enable_transition_deadline_ns"] = Value::Null;
            report["enable_confirmed_ns"] = Value::Null;
            report["disable_attempt_ns"] = Value::Null;
            report["torque_off_confirmed_ns"] = Value::Null;
        }
    }
    report
}

fn save_report(file: &mut File, report: &Value) -> Result<bool, String> {
    serde_json::to_writer_pretty(&mut *file, report)
        .map_err(|e| format!("结果写入失败：{e}；原报告={report}"))?;
    file.write_all(b"\n")
        .and_then(|()| file.sync_all())
        .map_err(|e| format!("结果落盘失败：{e}；原报告={report}"))?;
    Ok(report["result"] == "SOFTWARE_PATH_PASS")
}

#[cfg(test)]
#[path = "supervised_mixed/tests.rs"]
mod tests;

pub(super) fn main_entry(raw: impl Iterator<Item = String>, position_hold: bool) -> ExitCode {
    let mode = if position_hold {
        Mode::PositionHold
    } else {
        Mode::Mixed
    };
    let args = match Args::parse(raw, mode) {
        Ok(Some(args)) => args,
        Ok(None) => {
            println!("{HELP}");
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            eprintln!("爸爸，参数错误：{error}\n{HELP}");
            return ExitCode::from(2);
        }
    };
    let prepared = (|| {
        let config = Config::load(&args)?;
        let observer = match config.mode {
            Mode::Mixed => Some(Observations::open(&args.log, config.wheel_ids)?),
            Mode::PositionHold => None,
        };
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&args.output)
            .map_err(|e| e.to_string())?;
        Ok::<_, String>((config, observer, file))
    })();
    let (config, observer, file) = match prepared {
        Ok(value) => value,
        Err(error) => {
            eprintln!("爸爸，前置拒绝：{error}");
            return ExitCode::from(2);
        }
    };
    match run(&args, config, file, observer) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("爸爸，发送模式失败：{error}");
            ExitCode::FAILURE
        }
    }
}
