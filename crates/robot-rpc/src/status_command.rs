//! 使能前只读核对 S1 实际命令镜像；一个线程服务全部总线，不接触正式命令流。

use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use iceoryx2::prelude::NodeBuilder;
use iceoryx2::service::ipc::Service;
use robot_core::time::{RobotClock, SystemClock};
use robot_msg::{markers, msg};
use robot_transport::iceoryx2::{ObserverMetadata, ObserverSubscriber, observer_subscriber};
use tokio::sync::watch;
use tonic::transport::Channel;

use super::Source;
use crate::client::layout::validate_state_layout;
use crate::proto::robot_control_client::RobotControlClient;
use crate::proto::{GetStateRequest, RobotState, S0Mode, S0SafetyState};

const FRESH_NS: u64 = 200_000_000;
const PREPARE_BUDGET: Duration = Duration::from_millis(500);
const POLL: Duration = Duration::from_millis(5);
const REOPEN_NS: u64 = 1_000_000_000;
const MAX_DRAIN: usize = 64;

#[derive(Clone)]
struct Command {
    metadata: ObserverMetadata,
    seq: u64,
    stamp_ns: u64,
    deadline_ns: u64,
    target: Vec<f64>,
}

#[derive(Default)]
struct Cache {
    command: Option<Command>,
    error: Option<String>,
    generation: u64,
    continuous: bool,
}

impl Cache {
    fn fail(&mut self, error: String) {
        self.generation = self.generation.saturating_add(1);
        self.error = Some(error);
        self.continuous = false;
    }

    fn accept(&mut self, command: Command) {
        if self.command.as_ref().is_some_and(|previous| {
            command.metadata.publisher_instance_id != previous.metadata.publisher_instance_id
                || previous.metadata.observer_seq.checked_add(1)
                    != Some(command.metadata.observer_seq)
                || command.metadata.dropped_frames != previous.metadata.dropped_frames
                || previous.seq.checked_add(1) != Some(command.seq)
                || command.stamp_ns <= previous.stamp_ns
        }) {
            self.fail("命令镜像换源、丢帧或序号/时间未推进".into());
        } else {
            self.continuous = self.command.is_some();
            self.error = None;
        }
        self.command = Some(command);
    }
}

/// 只有普通观测线程持有 IPC 对象；异步轮询和所有浏览器共用有限最新值缓存。
#[derive(Clone)]
pub(super) struct Mirrors {
    sources: Arc<Vec<Source>>,
    caches: Arc<Mutex<Vec<Cache>>>,
}

impl Mirrors {
    pub(super) fn new(sources: Vec<Source>) -> Self {
        Self {
            caches: Arc::new(Mutex::new(
                sources.iter().map(|_| Cache::default()).collect(),
            )),
            sources: Arc::new(sources),
        }
    }

    pub(super) fn start(
        &self,
        shutdown: watch::Receiver<bool>,
    ) -> std::io::Result<JoinHandle<Result<(), String>>> {
        let mirrors = self.clone();
        std::thread::Builder::new()
            .name("robot-command-web".into())
            .spawn(move || {
                let result = mirrors.run(&shutdown);
                if let Err(error) = &result
                    && let Ok(mut caches) = mirrors.caches.lock()
                {
                    for cache in &mut *caches {
                        cache.fail(error.clone());
                    }
                }
                result
            })
    }

    fn run(&self, shutdown: &watch::Receiver<bool>) -> Result<(), String> {
        let mut config = iceoryx2::config::Config::global_config().clone();
        config.global.node.cleanup_dead_nodes_on_creation = false;
        config.global.node.cleanup_dead_nodes_on_destruction = false;
        let node = NodeBuilder::new()
            .config(&config)
            .create::<Service>()
            .map_err(|error| format!("命令观测节点创建失败：{error:?}"))?;
        let mut subscribers: Vec<Option<ObserverSubscriber<markers::JointCommand>>> =
            self.sources.iter().map(|_| None).collect();
        let mut reopen_at = vec![0; self.sources.len()];
        let clock = SystemClock;
        while !*shutdown.borrow() && shutdown.has_changed().is_ok() {
            for (index, source) in self.sources.iter().enumerate() {
                if *shutdown.borrow() {
                    return Ok(());
                }
                if source.motion.is_none() || source.enable_alignment_tolerance_rad.is_none() {
                    continue;
                }
                if subscribers[index].is_none() && clock.now().0 >= reopen_at[index] {
                    reopen_at[index] = clock.now().0.saturating_add(REOPEN_NS);
                    let topic = format!("/{}/s1/{}/command", source.robot_id, source.hardware_id);
                    match observer_subscriber::<markers::JointCommand>(&node, &topic) {
                        Ok(subscriber) => subscribers[index] = Some(subscriber),
                        Err(error) => {
                            self.fail(index, format!("S1命令镜像尚不可用：{error}"))?;
                        }
                    }
                }
                let Some(subscriber) = subscribers[index].as_mut() else {
                    continue;
                };
                for _ in 0..MAX_DRAIN {
                    let sample = match subscriber.try_recv() {
                        Ok(Some(sample)) => sample,
                        Ok(None) => break,
                        Err(error) => {
                            self.fail(index, format!("命令镜像读取失败：{error}"))?;
                            subscribers[index] = None;
                            break;
                        }
                    };
                    let command = sample
                        .view()
                        .map_err(|error| error.to_string())
                        .and_then(|view| decode(source, &view, sample.metadata(), clock.now().0));
                    let mut caches = self.caches.lock().map_err(|_| "命令镜像缓存锁异常")?;
                    match command {
                        Ok(command) => caches[index].accept(command),
                        Err(error) => caches[index].fail(error),
                    }
                }
            }
            std::thread::sleep(POLL);
        }
        Ok(())
    }

    fn fail(&self, index: usize, error: String) -> Result<(), String> {
        self.caches.lock().map_err(|_| "命令镜像缓存锁异常")?[index].fail(error);
        Ok(())
    }

    fn latest(&self, source: &Source, now_ns: u64) -> Result<(Command, u64), String> {
        let index = self
            .sources
            .iter()
            .position(|item| item.id == source.id)
            .ok_or("缺少本总线命令镜像")?;
        let caches = self.caches.lock().map_err(|_| "命令镜像缓存锁异常")?;
        let cache = &caches[index];
        if let Some(error) = &cache.error {
            return Err(error.clone());
        }
        if !cache.continuous {
            return Err("等待连续两帧S1命令镜像".into());
        }
        let command = cache.command.as_ref().ok_or("尚未收到S1命令镜像")?;
        fresh(command.stamp_ns, now_ns)?;
        if command.deadline_ns <= now_ns {
            return Err("S1命令镜像已经过期".into());
        }
        Ok((command.clone(), cache.generation))
    }

    pub(super) fn check(
        &self,
        source: &Source,
        state: &RobotState,
        now_ns: u64,
    ) -> Result<(), String> {
        check_state(source, state, now_ns)?;
        let (command, _) = self.latest(source, now_ns)?;
        aligned(source, &state.position_rad, &command.target)
    }
}

fn decode(
    source: &Source,
    view: &robot_transport::MessageView<'_, markers::JointCommand>,
    metadata: ObserverMetadata,
    now_ns: u64,
) -> Result<Command, String> {
    let body = view.body.get();
    let count = source.layout.joints.len();
    let target = body.target().ok_or("命令目标缺失")?;
    if view.header.source_id() != 2
        || body.layout_id() != source.layout.layout_id
        || body.source_task_id() != view.header.seq()
        || body.mode()
            != if source.mixed_raw {
                msg::ControlMode::MixedPositionRawVelocity
            } else {
                msg::ControlMode::Position
            }
        || body.on_timeout() != msg::StaleAction::Hold
        || target.len() != count
        || body.deadline_ns() <= view.header.stamp_ns()
    {
        return Err("命令镜像来源、布局、模式、维度或期限无效".into());
    }
    fresh(view.header.stamp_ns(), now_ns)?;
    if target.iter().any(|value| !finite_f32(value)) {
        return Err("命令目标含非有限或不可表示值".into());
    }
    if source.mixed_raw {
        let raw = body.raw_velocities().ok_or("混合命令缺少raw速度")?;
        if raw.len() != count
            || raw.iter().any(|value| value != 0)
            || target
                .iter()
                .enumerate()
                .any(|(slot, value)| wheel(source, slot) && value != 0.0)
        {
            return Err("混合命令必须保留完整三轮零目标和全槽raw零速".into());
        }
    } else if body
        .raw_velocities()
        .is_some_and(|raw| raw.iter().any(|value| value != 0))
    {
        return Err("位置命令不能携带非零raw速度".into());
    }
    let target: Vec<_> = target.iter().collect();
    source
        .motion
        .as_ref()
        .ok_or("缺少运动范围配置")?
        .check_hold_positions(source, &target)?;
    Ok(Command {
        metadata,
        seq: view.header.seq(),
        stamp_ns: view.header.stamp_ns(),
        deadline_ns: body.deadline_ns(),
        target,
    })
}

fn fresh(stamp_ns: u64, now_ns: u64) -> Result<(), String> {
    if stamp_ns == 0
        || now_ns
            .checked_sub(stamp_ns)
            .is_none_or(|age| age > FRESH_NS)
    {
        return Err("使能证据来自未来、时间为零或已超过200毫秒".into());
    }
    Ok(())
}

fn wheel(source: &Source, slot: usize) -> bool {
    source.mixed_raw && matches!(source.layout.motor_ids[slot], 7..=9)
}

fn finite_f32(value: f64) -> bool {
    value.is_finite() && value.abs() <= f64::from(f32::MAX)
}

fn check_state(source: &Source, state: &RobotState, now_ns: u64) -> Result<(), String> {
    if state.robot_id != source.robot_id || state.hardware_id != source.hardware_id {
        return Err("使能状态身份与配置不一致".into());
    }
    validate_state_layout(&source.layout, state).map_err(|error| error.to_string())?;
    fresh(state.stamp_ns, now_ns)?;
    if state.mode != S0Mode::Safe as i32
        || state.safety_state != S0SafetyState::TorqueOffConfirmed as i32
        || state.fault_code != 0
    {
        return Err("使能前必须为Safe、确认断扭且无故障；急停需单独解除".into());
    }
    if state.position_rad.len() != source.layout.joints.len()
        || state.position_rad.iter().any(|&value| !finite_f32(value))
    {
        return Err("使能状态位置维度或有限值无效".into());
    }
    source
        .motion
        .as_ref()
        .ok_or("缺少运动范围配置")?
        .check_hold_positions(source, &state.position_rad)?;
    Ok(())
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "比较实际S0 f32编码，调用前验证有限及可表示值"
)]
fn aligned(source: &Source, state: &[f64], target: &[f64]) -> Result<(), String> {
    let tolerance = source
        .enable_alignment_tolerance_rad
        .ok_or("未配置使能原位对齐容差")?;
    for (slot, (&actual, &expected)) in state.iter().zip(target).enumerate() {
        if !wheel(source, slot)
            && (f64::from(actual as f32) - f64::from(expected as f32)).abs() > tolerance
        {
            return Err(format!("{}尚未原位对齐", source.layout.joints[slot]));
        }
    }
    Ok(())
}

/// 这是派发前的有限只读证据；最终使能仍由S0原安全门裁决，不宣称原子握手。
pub(super) async fn prepare_enable(
    source: &Source,
    mirrors: &Mirrors,
    client: &mut RobotControlClient<Channel>,
    clock: &dyn RobotClock,
    timeout: Duration,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<u64, String> {
    let budget = PREPARE_BUDGET.min(timeout);
    let probe = async {
        let (baseline, generation_at_start) = mirrors.latest(source, clock.now().0)?;
        let mut first: Option<(RobotState, Command, u64)> = None;
        loop {
            let mut request = tonic::Request::new(GetStateRequest {
                hardware_id: source.hardware_id.clone(),
            });
            request.set_timeout(budget);
            let state = client
                .get_state(request)
                .await
                .map_err(|error| format!("使能前状态读取失败，未发送：{error}"))?
                .into_inner();
            check_state(source, &state, clock.now().0)?;
            // 让S1有机会发布包含本次新状态的保持目标，不加新命令或RPC。
            tokio::time::sleep(POLL).await;
            let (command, generation) = mirrors.latest(source, clock.now().0)?;
            if generation != generation_at_start
                || command.metadata.publisher_instance_id != baseline.metadata.publisher_instance_id
            {
                return Err("使能核对期间命令源发生变化，未发送".into());
            }
            if let Some((previous, old_command, old_generation)) = &first {
                if generation != *old_generation
                    || command.metadata.publisher_instance_id
                        != old_command.metadata.publisher_instance_id
                    || command.metadata.dropped_frames != old_command.metadata.dropped_frames
                    || state.stamp_ns < previous.stamp_ns
                    || command.seq < old_command.seq
                {
                    return Err("使能核对期间换源、丢帧或状态时间回退，未发送".into());
                }
                if state.stamp_ns > previous.stamp_ns
                    && command.seq > old_command.seq
                    && command.stamp_ns >= state.stamp_ns
                    && aligned(source, &state.position_rad, &command.target).is_ok()
                    && aligned(source, &previous.position_rad, &state.position_rad).is_ok()
                {
                    check_state(source, &state, clock.now().0)?;
                    return Ok(state.stamp_ns);
                }
            } else if command.stamp_ns >= state.stamp_ns
                && aligned(source, &state.position_rad, &command.target).is_ok()
            {
                first = Some((state, command, generation));
            }
        }
    };
    tokio::select! {
        _ = shutdown.changed() => Err("Web关闭，未发送使能".into()),
        result = tokio::time::timeout(budget, probe) => result.map_err(|_| "500毫秒内未取得两份推进且原位对齐的证据，未发送使能".to_owned())?,
    }
}

#[cfg(test)]
#[path = "status_command_tests.rs"]
mod tests;
