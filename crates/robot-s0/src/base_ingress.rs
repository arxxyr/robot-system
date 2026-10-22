//! 底盘输入的纯有界状态机；不接 IPC、RPC、writer 或硬件。
//!
//! 来源是样本实际关联的完整 Node/Port，仍不构成密码学认证。生命周期事件由
//! 受信运行时投影；本模块不能验证调用者权限或物理 Ack。输出还须与真实安全/权限门相与。

use std::num::NonZeroU64;

use robot_core::adapter::{ControlModeRepr, JointCommandBuf};
use robot_core::time::{DurationNs, MonoRawNs};
use robot_core::wheel_velocity_profile::WheelVelocityProfile;
use robot_transport::{PublisherNodeId, PublisherOrigin, PublisherPortId};

/// 生产者命令有效期的严格上限。
pub const COMMAND_TTL_MAX_NS: u64 = 100_000_000;

/// 显式配置的来源；零是合法配置值，未知由 `Option` 表达。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConfiguredSourceId(u32);

impl ConfiguredSourceId {
    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// 显式输入合同，没有隐式几何或来源配置。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BaseIngressContract {
    source: ConfiguredSourceId,
    layout_id: u64,
    profile: WheelVelocityProfile,
    heartbeat_timeout_ns: u64,
}

impl BaseIngressContract {
    pub fn new(
        source: ConfiguredSourceId,
        layout_id: u64,
        profile: WheelVelocityProfile,
        heartbeat_timeout: DurationNs,
    ) -> Result<Self, IngressReject> {
        match heartbeat_timeout.as_nanos() {
            0 => Err(IngressReject::InvalidHeartbeatBudget),
            heartbeat_timeout_ns => Ok(Self {
                source,
                layout_id,
                profile,
                heartbeat_timeout_ns,
            }),
        }
    }
}

/// 已解码的生产者帧字段；时间来自生产者，不能用接收时间代替。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProducerFrame {
    pub source_id: u32,
    pub seq: u64,
    pub stamp: MonoRawNs,
}

/// 接入包必须保真映射 wire 值，不把未知值映成 Zero。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimeoutIntent {
    Zero,
    Hold,
    Stop,
    Unknown(u8),
}

/// 只借用输入数组；成功校验后只复制固定三项。
#[derive(Clone, Copy, Debug)]
pub struct BaseCommandInput<'a> {
    pub origin: Option<PublisherOrigin>,
    pub header: ProducerFrame,
    pub layout_id: u64,
    pub mode: u8,
    pub targets_rad_s: &'a [f64],
    pub deadline: MonoRawNs,
    pub on_timeout: TimeoutIntent,
    pub source_task_id: u64,
}

/// 心跳 body 三个身份/序号/时间字段必须逐项等于 header。
#[derive(Clone, Copy, Debug)]
pub struct HeartbeatInput {
    pub origin: Option<PublisherOrigin>,
    pub header: ProducerFrame,
    pub body: ProducerFrame,
    pub tick_rate_hz: f64,
}

/// 重建链的非零关联票据；仅本状态机 checked 生成。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RebindTicket(NonZeroU64);

impl RebindTicket {
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// 运行时真实关闭链的结果投影；这些字段本身不是权限或物理验证。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DisabledEvidence {
    pub disabled: bool,
    pub owner_retired: bool,
    pub torque_off_confirmed: bool,
}

/// 真实连接代次的显式投影，必须与重建预期一致。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConnectionEpoch(u64);

impl ConnectionEpoch {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }
}

/// 生命周期实际结果，不能把 Unknown 或失败当成功。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompletionOutcome {
    Committed,
    Failed,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IngressPhase {
    Disabled,
    RebindPending,
    ReadyToEnable,
    Active,
    Lost,
    RebindRequired,
    Exhausted,
}

/// 首次耗尽原因保持粘性；只读分类不构成恢复或输出许可。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExhaustionReason {
    Counter,
    Ticket,
}

impl ExhaustionReason {
    const fn rejection(self) -> IngressReject {
        match self {
            Self::Counter => IngressReject::CounterExhausted,
            Self::Ticket => IngressReject::TicketExhausted,
        }
    }
}

/// 固定错误类型；热路径不构造字符串或动态诊断容器。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IngressReject {
    InvalidHeartbeatBudget,
    UnknownOrigin,
    ConfiguredSourceMismatch,
    HeartbeatHeaderBodyMismatch,
    InvalidHeartbeatRate,
    InvalidLayout,
    InvalidMode,
    UnsupportedTimeout,
    WrongTargetCount,
    NonFiniteTarget,
    TargetOutsideProfile,
    TargetOutsideF32,
    InvalidCommandTtl,
    FutureStamp,
    ExpiredCommand,
    ExpiredHeartbeat,
    ClockArithmeticOverflow,
    ClockRegression,
    ProducerSeqNotIncreasing,
    ProducerStampRegression,
    BelowBindingCutoff,
    BelowRecoveryCutoff,
    BelowKnownFence,
    PortAlias,
    IdentityChanged,
    RetiredPair,
    WrongLifecyclePhase,
    DisabledOrRetirementUnconfirmed,
    WrongRebindTicket,
    WrongConnectionEpoch,
    InternalZeroAckUnconfirmed,
    AckAlreadyObserved,
    EnableResultUnconfirmed,
    CounterExhausted,
    TicketExhausted,
    ControlLost,
    GateClosed,
}

/// 候选条件或关闭要求；不是硬件成功证明或 authority token。
#[must_use]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReducerEffect {
    Unchanged,
    Staged,
    Accepted,
    MayRequestEnable(RebindTicket),
    EnteredActive,
    Recovered,
    EnterSafe(IngressReject),
    Rejected(IngressReject),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionIdentity {
    pub node: PublisherNodeId,
    pub command_port: PublisherPortId,
    pub heartbeat_port: PublisherPortId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Cursor {
    seq: u64,
    stamp: MonoRawNs,
}

impl From<ProducerFrame> for Cursor {
    fn from(value: ProducerFrame) -> Self {
        Self {
            seq: value.seq,
            stamp: value.stamp,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SessionSlot {
    node: PublisherNodeId,
    command_port: Option<PublisherPortId>,
    heartbeat_port: Option<PublisherPortId>,
    command_cursor: Option<Cursor>,
    heartbeat_cursor: Option<Cursor>,
}

impl SessionSlot {
    fn identity(self) -> Option<SessionIdentity> {
        Some(SessionIdentity {
            node: self.node,
            command_port: self.command_port?,
            heartbeat_port: self.heartbeat_port?,
        })
    }

    fn cursor(self, origin: PublisherOrigin) -> Option<Cursor> {
        match (self.node == origin.node_id(), origin.publisher_id()) {
            (true, port) if self.command_port == Some(port) => self.command_cursor,
            (true, port) if self.heartbeat_port == Some(port) => self.heartbeat_cursor,
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ValidCommand {
    origin: PublisherOrigin,
    cursor: Cursor,
    targets: [f32; 3],
    deadline: MonoRawNs,
    task: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ValidHeartbeat {
    origin: PublisherOrigin,
    cursor: Cursor,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LostFence {
    accepted_floor: u64,
    cutoff: MonoRawNs,
}

/// 诊断快照没有转换成可执行 POD 的方法；公开字段不构成输出许可。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BaseIngressSnapshot {
    pub phase: IngressPhase,
    pub exhaustion_reason: Option<ExhaustionReason>,
    pub accepted_counter: Option<NonZeroU64>,
    pub ticket: Option<RebindTicket>,
    pub current_identity: Option<SessionIdentity>,
    pub retired_identity: Option<SessionIdentity>,
    pub command_origin: Option<PublisherOrigin>,
    pub heartbeat_origin: Option<PublisherOrigin>,
    pub targets_rad_s: Option<[f32; 3]>,
    pub command_stamp: Option<MonoRawNs>,
    pub command_deadline: Option<MonoRawNs>,
    pub task_association: Option<u64>,
    pub binding_cutoff: Option<MonoRawNs>,
    pub loss_cutoff: Option<MonoRawNs>,
    pub loss_counter_floor: Option<u64>,
    pub enable_committed: bool,
}

/// 只有一个当前 Node/两个端口、一个退休完整身份、各一暂存样本。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BaseIngress {
    contract: BaseIngressContract,
    phase: IngressPhase,
    exhaustion_reason: Option<ExhaustionReason>,
    current: Option<SessionSlot>,
    retired: Option<SessionSlot>,
    staged_command: Option<ValidCommand>,
    staged_heartbeat: Option<ValidHeartbeat>,
    command: Option<ValidCommand>,
    heartbeat: Option<ValidHeartbeat>,
    accepted_counter: u64,
    ticket_counter: u64,
    ticket: Option<RebindTicket>,
    connection: Option<ConnectionEpoch>,
    binding_cutoff: Option<MonoRawNs>,
    zero_ack_seen: bool,
    enable_committed: bool,
    loss: Option<LostFence>,
}

#[derive(Clone, Copy)]
enum Input {
    Command(ValidCommand),
    Heartbeat(ValidHeartbeat),
}

impl Input {
    fn origin(self) -> PublisherOrigin {
        match self {
            Self::Command(value) => value.origin,
            Self::Heartbeat(value) => value.origin,
        }
    }

    fn cursor(self) -> Cursor {
        match self {
            Self::Command(value) => value.cursor,
            Self::Heartbeat(value) => value.cursor,
        }
    }
}

impl BaseIngress {
    #[must_use]
    pub const fn new(contract: BaseIngressContract) -> Self {
        Self {
            contract,
            phase: IngressPhase::Disabled,
            exhaustion_reason: None,
            current: None,
            retired: None,
            staged_command: None,
            staged_heartbeat: None,
            command: None,
            heartbeat: None,
            accepted_counter: 0,
            ticket_counter: 0,
            ticket: None,
            connection: None,
            binding_cutoff: None,
            zero_ack_seen: false,
            enable_committed: false,
            loss: None,
        }
    }

    /// 仅创建候选屏障；关闭证据和真实调用方权限由接入包负责验证。
    pub fn begin_explicit_enable(
        &mut self,
        evidence: DisabledEvidence,
        connection: ConnectionEpoch,
        now: MonoRawNs,
    ) -> Result<RebindTicket, IngressReject> {
        match self.phase {
            IngressPhase::Disabled | IngressPhase::RebindRequired => {}
            IngressPhase::Exhausted => return Err(self.exhausted_rejection()),
            _ => return Err(IngressReject::WrongLifecyclePhase),
        }
        if !(evidence.disabled && evidence.owner_retired && evidence.torque_off_confirmed) {
            return Err(IngressReject::DisabledOrRetirementUnconfirmed);
        }
        if self.producer_clock_ahead(now) {
            return Err(IngressReject::ClockRegression);
        }
        let Some(next) = self.ticket_counter.checked_add(1).and_then(NonZeroU64::new) else {
            self.exhaust(ExhaustionReason::Ticket);
            return Err(IngressReject::TicketExhausted);
        };
        self.retire_current();
        self.clear_binding();
        let ticket = RebindTicket(next);
        self.ticket_counter = next.get();
        self.ticket = Some(ticket);
        self.connection = Some(connection);
        self.binding_cutoff = Some(now);
        self.phase = IngressPhase::RebindPending;
        Ok(ticket)
    }

    /// 只有首次同票据/连接成功 Ack 才建立时间下限并丢弃 Ack 前暂存。
    pub fn observe_internal_zero_ack(
        &mut self,
        ticket: RebindTicket,
        connection: ConnectionEpoch,
        outcome: CompletionOutcome,
        now: MonoRawNs,
    ) -> ReducerEffect {
        let validation = match self.phase {
            IngressPhase::RebindPending if self.ticket != Some(ticket) => {
                Err(IngressReject::WrongRebindTicket)
            }
            IngressPhase::RebindPending if self.connection != Some(connection) => {
                Err(IngressReject::WrongConnectionEpoch)
            }
            IngressPhase::RebindPending if self.zero_ack_seen => {
                Err(IngressReject::AckAlreadyObserved)
            }
            IngressPhase::RebindPending if outcome != CompletionOutcome::Committed => {
                Err(IngressReject::InternalZeroAckUnconfirmed)
            }
            IngressPhase::RebindPending
                if self.binding_cutoff.is_some_and(|cutoff| now < cutoff) =>
            {
                Err(IngressReject::ClockRegression)
            }
            IngressPhase::RebindPending => Ok(()),
            IngressPhase::Exhausted => Err(self.exhausted_rejection()),
            _ => Err(IngressReject::WrongLifecyclePhase),
        };
        if let Err(error) = validation {
            return ReducerEffect::Rejected(error);
        }
        // 暂存游标未 accepted；丢弃它们，不改退休槽里的真实已接受 floor。
        self.current = None;
        self.staged_command = None;
        self.staged_heartbeat = None;
        self.binding_cutoff = Some(now);
        self.zero_ack_seen = true;
        ReducerEffect::Staged
    }

    /// 真实 Enable 成功事件只在同票据 Ready 且现在仍新鲜时开候选门。
    pub fn observe_enable_committed(
        &mut self,
        ticket: RebindTicket,
        outcome: CompletionOutcome,
        now: MonoRawNs,
    ) -> ReducerEffect {
        match self.phase {
            IngressPhase::ReadyToEnable if self.ticket != Some(ticket) => {
                ReducerEffect::Rejected(IngressReject::WrongRebindTicket)
            }
            IngressPhase::ReadyToEnable if outcome != CompletionOutcome::Committed => {
                ReducerEffect::Rejected(IngressReject::EnableResultUnconfirmed)
            }
            IngressPhase::ReadyToEnable if !self.fresh(now) => self.on_control_lost(now),
            IngressPhase::ReadyToEnable => {
                self.enable_committed = true;
                self.phase = IngressPhase::Active;
                ReducerEffect::EnteredActive
            }
            IngressPhase::Exhausted => ReducerEffect::Rejected(self.exhausted_rejection()),
            _ => ReducerEffect::Rejected(IngressReject::WrongLifecyclePhase),
        }
    }

    pub fn accept_command(&mut self, input: BaseCommandInput<'_>, now: MonoRawNs) -> ReducerEffect {
        if self.phase == IngressPhase::Exhausted {
            return ReducerEffect::Rejected(self.exhausted_rejection());
        }
        match self.validate_command(input, now) {
            Ok(value) => self.accept(Input::Command(value), now),
            Err(error) => ReducerEffect::Rejected(error),
        }
    }

    pub fn accept_heartbeat(&mut self, input: HeartbeatInput, now: MonoRawNs) -> ReducerEffect {
        if self.phase == IngressPhase::Exhausted {
            return ReducerEffect::Rejected(self.exhausted_rejection());
        }
        match self.validate_heartbeat(input, now) {
            Ok(value) => self.accept(Input::Heartbeat(value), now),
            Err(error) => ReducerEffect::Rejected(error),
        }
    }

    /// 重复 Lost 不移动 floor/cutoff；Ready 来源保持未 committed。
    pub fn on_control_lost(&mut self, now: MonoRawNs) -> ReducerEffect {
        match self.phase {
            IngressPhase::Active | IngressPhase::ReadyToEnable => {
                // 不用倒退的调用时刻创建较低恢复屏障，也不悄悄钳正时钟。
                if self.producer_clock_ahead(now) {
                    return self.require_rebind(IngressReject::ClockRegression);
                }
                self.loss = Some(LostFence {
                    accepted_floor: self.accepted_counter,
                    cutoff: now,
                });
                self.phase = IngressPhase::Lost;
                ReducerEffect::EnterSafe(IngressReject::ControlLost)
            }
            IngressPhase::Lost => ReducerEffect::Unchanged,
            IngressPhase::Exhausted => ReducerEffect::Rejected(self.exhausted_rejection()),
            _ => ReducerEffect::Rejected(IngressReject::WrongLifecyclePhase),
        }
    }

    pub fn on_tick(&mut self, now: MonoRawNs) -> ReducerEffect {
        match self.phase {
            IngressPhase::Active | IngressPhase::ReadyToEnable if !self.fresh(now) => {
                self.on_control_lost(now)
            }
            IngressPhase::Exhausted => ReducerEffect::Rejected(self.exhausted_rejection()),
            _ => ReducerEffect::Unchanged,
        }
    }

    /// 显式 Disable 或外部安全 inhibit 投影；不会清任何外部保护或授权。
    pub fn observe_disabled(&mut self) -> ReducerEffect {
        if self.phase == IngressPhase::Exhausted {
            return ReducerEffect::Rejected(self.exhausted_rejection());
        }
        self.retire_current();
        self.clear_binding();
        self.phase = IngressPhase::Disabled;
        ReducerEffect::EnterSafe(IngressReject::GateClosed)
    }

    /// 唯一 POD 转换入口；诊断快照不可转换，返回值仍须真实 authority 守门。
    pub fn gated_command(&self, now: MonoRawNs) -> Result<JointCommandBuf, IngressReject> {
        if self.phase != IngressPhase::Active || !self.enable_committed {
            return Err(IngressReject::GateClosed);
        }
        let command = self.command.ok_or(IngressReject::GateClosed)?;
        Self::command_fresh(command, now)?;
        self.heartbeat_fresh(self.heartbeat.ok_or(IngressReject::GateClosed)?, now)?;
        let seq = NonZeroU64::new(self.accepted_counter).ok_or(IngressReject::GateClosed)?;
        let mut buffer = JointCommandBuf {
            mode: ControlModeRepr::Velocity,
            selection: robot_core::adapter::JointSelection::All,
            stamp_ns: command.cursor.stamp.0,
            deadline_ns: command.deadline.0,
            joint_count: 3,
            seq: seq.get(),
            ..JointCommandBuf::default()
        };
        buffer.velocities[..3].copy_from_slice(&command.targets);
        Ok(buffer)
    }

    #[must_use]
    pub fn snapshot(&self) -> BaseIngressSnapshot {
        BaseIngressSnapshot {
            phase: self.phase,
            exhaustion_reason: self.exhaustion_reason,
            accepted_counter: NonZeroU64::new(self.accepted_counter),
            ticket: self.ticket,
            // Pending 的完整候选仍非 accepted，不与上轮诊断命令冒作同一绑定。
            current_identity: match self.phase {
                IngressPhase::ReadyToEnable | IngressPhase::Active | IngressPhase::Lost => {
                    self.current.and_then(SessionSlot::identity)
                }
                _ => None,
            },
            retired_identity: self.retired.and_then(SessionSlot::identity),
            command_origin: self.command.map(|value| value.origin),
            heartbeat_origin: self.heartbeat.map(|value| value.origin),
            targets_rad_s: self.command.map(|value| value.targets),
            command_stamp: self.command.map(|value| value.cursor.stamp),
            command_deadline: self.command.map(|value| value.deadline),
            task_association: self.command.map(|value| value.task),
            binding_cutoff: self.binding_cutoff,
            loss_cutoff: self.loss.map(|value| value.cutoff),
            loss_counter_floor: self.loss.map(|value| value.accepted_floor),
            enable_committed: self.enable_committed,
        }
    }

    fn validate_command(
        &self,
        input: BaseCommandInput<'_>,
        now: MonoRawNs,
    ) -> Result<ValidCommand, IngressReject> {
        let origin = input.origin.ok_or(IngressReject::UnknownOrigin)?;
        self.validate_source(input.header)?;
        if input.layout_id != self.contract.layout_id {
            return Err(IngressReject::InvalidLayout);
        }
        if input.mode != ControlModeRepr::Velocity as u8 {
            return Err(IngressReject::InvalidMode);
        }
        if input.on_timeout != TimeoutIntent::Zero {
            return Err(IngressReject::UnsupportedTimeout);
        }
        let values: &[f64; 3] = input
            .targets_rad_s
            .try_into()
            .map_err(|_| IngressReject::WrongTargetCount)?;
        let mut targets = [0.0; 3];
        for (target, value) in targets.iter_mut().zip(values) {
            if !value.is_finite() {
                return Err(IngressReject::NonFiniteTarget);
            }
            if value.abs() > self.contract.profile.max_wheel_rad_s() {
                return Err(IngressReject::TargetOutsideProfile);
            }
            if value.abs() > f64::from(f32::MAX) {
                return Err(IngressReject::TargetOutsideF32);
            }
            // 此处已经校验范围；转换后的实际值仍要单独核上限，禁止自限幅。
            #[allow(clippy::cast_possible_truncation)]
            let converted = *value as f32;
            if f64::from(converted).abs() > self.contract.profile.max_wheel_rad_s() {
                return Err(IngressReject::TargetOutsideProfile);
            }
            *target = converted;
        }
        let ttl = input
            .deadline
            .0
            .checked_sub(input.header.stamp.0)
            .ok_or(IngressReject::InvalidCommandTtl)?;
        if !(1..=COMMAND_TTL_MAX_NS).contains(&ttl) {
            return Err(IngressReject::InvalidCommandTtl);
        }
        let command = ValidCommand {
            origin,
            cursor: input.header.into(),
            targets,
            deadline: input.deadline,
            task: input.source_task_id,
        };
        Self::command_fresh(command, now)?;
        Ok(command)
    }

    fn validate_heartbeat(
        &self,
        input: HeartbeatInput,
        now: MonoRawNs,
    ) -> Result<ValidHeartbeat, IngressReject> {
        let origin = input.origin.ok_or(IngressReject::UnknownOrigin)?;
        self.validate_source(input.header)?;
        if input.header != input.body {
            return Err(IngressReject::HeartbeatHeaderBodyMismatch);
        }
        if !input.tick_rate_hz.is_finite() || input.tick_rate_hz <= 0.0 {
            return Err(IngressReject::InvalidHeartbeatRate);
        }
        let heartbeat = ValidHeartbeat {
            origin,
            cursor: input.header.into(),
        };
        self.heartbeat_fresh(heartbeat, now)?;
        Ok(heartbeat)
    }

    fn validate_source(&self, frame: ProducerFrame) -> Result<(), IngressReject> {
        if frame.source_id != self.contract.source.get() {
            return Err(IngressReject::ConfiguredSourceMismatch);
        }
        Ok(())
    }

    fn command_fresh(command: ValidCommand, now: MonoRawNs) -> Result<(), IngressReject> {
        if command.cursor.stamp > now {
            return Err(IngressReject::FutureStamp);
        }
        if now >= command.deadline {
            return Err(IngressReject::ExpiredCommand);
        }
        Ok(())
    }

    fn heartbeat_fresh(
        &self,
        heartbeat: ValidHeartbeat,
        now: MonoRawNs,
    ) -> Result<(), IngressReject> {
        let elapsed = now
            .0
            .checked_sub(heartbeat.cursor.stamp.0)
            .ok_or(IngressReject::FutureStamp)?;
        heartbeat
            .cursor
            .stamp
            .0
            .checked_add(self.contract.heartbeat_timeout_ns)
            .ok_or(IngressReject::ClockArithmeticOverflow)?;
        if elapsed >= self.contract.heartbeat_timeout_ns {
            return Err(IngressReject::ExpiredHeartbeat);
        }
        Ok(())
    }

    fn fresh(&self, now: MonoRawNs) -> bool {
        match (self.command, self.heartbeat) {
            (Some(command), Some(heartbeat)) => {
                Self::command_fresh(command, now).is_ok()
                    && self.heartbeat_fresh(heartbeat, now).is_ok()
            }
            _ => false,
        }
    }

    fn producer_clock_ahead(&self, now: MonoRawNs) -> bool {
        self.command
            .is_some_and(|command| command.cursor.stamp > now)
            || self
                .heartbeat
                .is_some_and(|heartbeat| heartbeat.cursor.stamp > now)
    }

    fn progress(&self, input: Input) -> Result<(), IngressReject> {
        let origin = input.origin();
        let cursor = input.cursor();
        if let Some(previous) = self.current.and_then(|slot| slot.cursor(origin)) {
            if cursor.seq <= previous.seq {
                return Err(IngressReject::ProducerSeqNotIncreasing);
            }
            if cursor.stamp < previous.stamp {
                return Err(IngressReject::ProducerStampRegression);
            }
        }
        if let Some(previous) = self.retired.and_then(|slot| slot.cursor(origin))
            && (cursor.seq <= previous.seq || cursor.stamp <= previous.stamp)
        {
            return Err(IngressReject::BelowKnownFence);
        }
        match self.phase {
            IngressPhase::RebindPending
                if self
                    .binding_cutoff
                    .is_some_and(|cutoff| cursor.stamp <= cutoff) =>
            {
                Err(IngressReject::BelowBindingCutoff)
            }
            IngressPhase::Lost if self.loss.is_some_and(|fence| cursor.stamp <= fence.cutoff) => {
                Err(IngressReject::BelowRecoveryCutoff)
            }
            _ => Ok(()),
        }
    }

    fn accept(&mut self, input: Input, now: MonoRawNs) -> ReducerEffect {
        match self.phase {
            IngressPhase::Exhausted => {
                return ReducerEffect::Rejected(self.exhausted_rejection());
            }
            IngressPhase::Disabled | IngressPhase::RebindRequired => {
                return ReducerEffect::Rejected(IngressReject::WrongLifecyclePhase);
            }
            _ => {}
        }
        if let Err(error) = self.progress(input) {
            return ReducerEffect::Rejected(error);
        }
        let slot = match self.prepare_slot(input) {
            Ok(slot) => slot,
            Err(IngressReject::IdentityChanged) => return self.invalidate_identity(),
            Err(error) => return ReducerEffect::Rejected(error),
        };
        match self.phase {
            IngressPhase::RebindPending => self.stage(input, slot, now),
            IngressPhase::Active | IngressPhase::ReadyToEnable if !self.fresh(now) => {
                self.on_control_lost(now)
            }
            IngressPhase::Active | IngressPhase::ReadyToEnable | IngressPhase::Lost => {
                self.commit(input, slot, now)
            }
            _ => ReducerEffect::Rejected(IngressReject::WrongLifecyclePhase),
        }
    }

    fn prepare_slot(&self, input: Input) -> Result<SessionSlot, IngressReject> {
        let origin = input.origin();
        let mut slot = self.current.unwrap_or(SessionSlot {
            node: origin.node_id(),
            command_port: None,
            heartbeat_port: None,
            command_cursor: None,
            heartbeat_cursor: None,
        });
        if slot.node != origin.node_id() {
            return Err(IngressReject::IdentityChanged);
        }
        let (port, other, cursor) = match input {
            Input::Command(_) => (
                &mut slot.command_port,
                slot.heartbeat_port,
                &mut slot.command_cursor,
            ),
            Input::Heartbeat(_) => (
                &mut slot.heartbeat_port,
                slot.command_port,
                &mut slot.heartbeat_cursor,
            ),
        };
        match *port {
            Some(previous) if previous != origin.publisher_id() => {
                return Err(IngressReject::IdentityChanged);
            }
            None if other == Some(origin.publisher_id()) => return Err(IngressReject::PortAlias),
            _ => {}
        }
        *port = Some(origin.publisher_id());
        *cursor = Some(input.cursor());
        Ok(slot)
    }

    fn stage(&mut self, input: Input, slot: SessionSlot, now: MonoRawNs) -> ReducerEffect {
        let command = match input {
            Input::Command(value) => Some(value),
            Input::Heartbeat(_) => self.staged_command,
        };
        let heartbeat = match input {
            Input::Heartbeat(value) => Some(value),
            Input::Command(_) => self.staged_heartbeat,
        };
        if self.zero_ack_seen
            && let (Some(command), Some(heartbeat), Some(identity)) =
                (command, heartbeat, slot.identity())
        {
            if self.retired.and_then(SessionSlot::identity) == Some(identity) {
                return ReducerEffect::Rejected(IngressReject::RetiredPair);
            }
            if Self::command_fresh(command, now).is_ok()
                && self.heartbeat_fresh(heartbeat, now).is_ok()
            {
                let Some(next) = self
                    .accepted_counter
                    .checked_add(1)
                    .and_then(NonZeroU64::new)
                else {
                    self.exhaust(ExhaustionReason::Counter);
                    return ReducerEffect::EnterSafe(IngressReject::CounterExhausted);
                };
                self.current = Some(slot);
                self.command = Some(command);
                self.heartbeat = Some(heartbeat);
                self.accepted_counter = next.get();
                self.staged_command = None;
                self.staged_heartbeat = None;
                self.phase = IngressPhase::ReadyToEnable;
                return self.ticket.map_or(
                    ReducerEffect::EnterSafe(IngressReject::GateClosed),
                    ReducerEffect::MayRequestEnable,
                );
            }
        }
        self.current = Some(slot);
        self.staged_command = command;
        self.staged_heartbeat = heartbeat;
        ReducerEffect::Staged
    }

    fn commit(&mut self, input: Input, slot: SessionSlot, now: MonoRawNs) -> ReducerEffect {
        match input {
            Input::Command(value) => {
                let Some(next) = self
                    .accepted_counter
                    .checked_add(1)
                    .and_then(NonZeroU64::new)
                else {
                    self.exhaust(ExhaustionReason::Counter);
                    return ReducerEffect::EnterSafe(IngressReject::CounterExhausted);
                };
                self.command = Some(value);
                self.accepted_counter = next.get();
            }
            Input::Heartbeat(value) => self.heartbeat = Some(value),
        }
        self.current = Some(slot);
        match self.loss {
            Some(fence)
                if self.phase == IngressPhase::Lost
                    && self.accepted_counter > fence.accepted_floor
                    && self
                        .command
                        .is_some_and(|value| value.cursor.stamp > fence.cutoff)
                    && self
                        .heartbeat
                        .is_some_and(|value| value.cursor.stamp > fence.cutoff)
                    && self.fresh(now) =>
            {
                self.loss = None;
                if self.enable_committed {
                    self.phase = IngressPhase::Active;
                    ReducerEffect::Recovered
                } else {
                    self.phase = IngressPhase::ReadyToEnable;
                    self.ticket.map_or(
                        ReducerEffect::EnterSafe(IngressReject::GateClosed),
                        ReducerEffect::MayRequestEnable,
                    )
                }
            }
            _ => ReducerEffect::Accepted,
        }
    }

    fn retire_current(&mut self) {
        // Pending 的 Partial/暂存游标从未 accepted，不能污染唯一退休 floor。
        if self.phase != IngressPhase::RebindPending
            && let Some(slot) = self.current.filter(|slot| slot.identity().is_some())
        {
            self.retired = Some(slot);
        }
    }

    fn clear_binding(&mut self) {
        self.current = None;
        self.staged_command = None;
        self.staged_heartbeat = None;
        self.zero_ack_seen = false;
        self.enable_committed = false;
        self.ticket = None;
        self.connection = None;
        self.binding_cutoff = None;
        self.loss = None;
    }

    fn invalidate_identity(&mut self) -> ReducerEffect {
        self.require_rebind(IngressReject::IdentityChanged)
    }

    fn require_rebind(&mut self, reason: IngressReject) -> ReducerEffect {
        if self.phase == IngressPhase::Exhausted {
            return ReducerEffect::Rejected(self.exhausted_rejection());
        }
        self.retire_current();
        self.clear_binding();
        self.phase = IngressPhase::RebindRequired;
        ReducerEffect::EnterSafe(reason)
    }

    fn exhausted_rejection(&self) -> IngressReject {
        self.exhaustion_reason
            .map_or(IngressReject::GateClosed, ExhaustionReason::rejection)
    }

    fn exhaust(&mut self, reason: ExhaustionReason) {
        // 不覆盖首次故障分类；Disable/重建清理也不触碰此字段。
        if self.exhaustion_reason.is_none() {
            self.exhaustion_reason = Some(reason);
        }
        self.enable_committed = false;
        self.phase = IngressPhase::Exhausted;
    }
}

#[cfg(test)]
mod tests;
