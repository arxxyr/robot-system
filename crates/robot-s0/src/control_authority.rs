//! S48 A2 控制权限原语：单个 `AtomicU64` 权威域上的同域 CAS，令牌化 Enable /
//! Clear 生命周期、inhibit 解除、有限 typed 安全转换与普通 IO 租约。
//!
//! 本模块只提供独立原语与测试，**尚未接入生产控制链**。A3 必须一次替换旧
//! `safety_word`、`safety_latched`、`inhibit` 的全部写者与读取点后它才成为权威；
//! 生产迁移之前，这里的任何许可都不代表真实机器人已被授权。本模块不执行任何
//! 文件或中间件操作：Finalize / Cleanup 许可只是调用方下一步的文件义务，
//! 必须由 A1 实际文件结果驱动后继。
//!
//! ## 权限字（位布局见 [`word`]）
//!
//! 低 24 位为状态（安全态、两类 EStop notice、`NORMAL_IO_ACTIVE`、Latch、
//! Phase、Disabled、Shutdown、Exhausted、六类 inhibit），高 40 位为权限 epoch。
//! 三个占位概念（Clear 事务、Enable 事务、执行中）全部由 Phase 派生，不另存
//! bool。权限 epoch 与动作 seq / trace epoch 分别命名、分别验证，不复用语义。
//!
//! ## 所有权合同
//!
//! - 所有 owner / 执行 / 修复 / cleanup / lease / 观测令牌字段私有、非 `Clone`，
//!   阶段转换按值消费并至多返回一个后继；失败返回输入令牌或唯一合法后继，
//!   不暗中丢失清理责任。跨 domain 先拒且不改任何位。
//! - 只读 `Copy` 视图（[`EnableStartView`] / [`EnableExecutionView`] /
//!   [`AuthoritySnapshot`]）不能构造 owner，也没有退休入口，因此消费旧 owner
//!   不可能释放后继事务。
//! - 任何安全撤销（EStop / Disable / Shutdown / typed 收紧 / inhibit 收紧）只
//!   递增权限 epoch 并收紧相应位，**不清 Phase**；占位只有当前 owner 真实退休
//!   才释放；丢弃 owner 保持保守卡住，`Drop` 不解除任何位。
//! - Enable 事务全程要求 Disabled = true；`finish_enable` 还要求
//!   `NORMAL_IO_ACTIVE` 为空，仍持有 wire 租约时不能进入终态。
//! - 授权类入口（`authorize_finalize` / `authorize_repair_cleanup`）与所有变更
//!   一样在同域 CAS 上线性化：校验通过后以无变化 CAS 提交授权，CAS 失败按最新
//!   值重判。重判的结果取决于当前状态：Finalizing 遇 Disable 而 Latch 仍为 false
//!   时仍按当前状态授权 Finalize（Disable 不是已提交 Clear 的回滚，只是运动继续
//!   被禁止）；遇 EStop（Latch = true）才拒绝 Finalize并要求修复为 `1`；修复许可
//!   遇任何撤销都因采样 epoch 或所需目标变化而重判。
//! - 权限 epoch 到达 [`PermissionEpoch::MAX`] 后没有下一代次：需要新代次的
//!   事件置 Exhausted + Disabled、永不回绕；Exhausted 下禁止新事务、权限提交
//!   与一切放宽，仍允许安全收紧、notice 消费、真实 IO 释放、执行结果记账及
//!   唯一 owner 退休。
//! - 热路径只做原子 load / CAS：无堆分配、无 `Arc`、无日志格式化、无 IO。
//!   CAS 循环是 lock-free，不承诺 wait-free 或固定重试次数。
//!
//! ## 与旧原语的关系
//!
//! `OutputSafetyState` wire 编码、`EStopOutcome` 语义与 EStop 安全态转换表沿用
//! `hard_rt_ctrl`；inhibit 位值与 `hard_rt_ctrl::inhibit` 一致，方便 A3 一比一
//! 迁移。本模块不修改旧原语。

#![allow(clippy::doc_markdown)]

use std::num::NonZeroU64;
use std::sync::atomic::{AtomicU64, Ordering};

use robot_core::executor_ctrl::{EStopOutcome, OutputSafetyState};

#[cfg(test)]
mod tests;
mod word;

use word::{
    Advance, DISABLED, EXHAUSTED, IPC_ESTOP_NOTICE, LATCH, NORMAL_IO_ACTIVE, SERIAL_ESTOP_PENDING,
    SHUTDOWN, Word,
};

/// 全局 domain 计数；只在非 RT 构造路径上以 checked CAS 递增，永不回绕。
static NEXT_DOMAIN: AtomicU64 = AtomicU64::new(1);

// ============================================================================
// 身份与编码类型
// ============================================================================

/// 权威域身份。构造时从全局计数分配，`0` 永不分配，耗尽即构造失败。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DomainId(NonZeroU64);

impl DomainId {
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// 40 位权限 epoch。与动作 seq、trace epoch 分别命名、分别验证。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PermissionEpoch(u64);

impl PermissionEpoch {
    /// 初始 epoch。
    pub const INITIAL: Self = Self(1);
    /// 最大 epoch；到达后没有下一代次。
    pub const MAX: Self = Self(word::EPOCH_MAX);

    /// 只接受 `1..=MAX`。
    #[must_use]
    pub const fn new(raw: u64) -> Option<Self> {
        if raw >= 1 && raw <= word::EPOCH_MAX {
            Some(Self(raw))
        } else {
            None
        }
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// 事务阶段；`7` 非法，本模块从不产生。
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Phase {
    Idle = 0,
    ClearPreparing = 1,
    ClearFinalizing = 2,
    EnableQueued = 3,
    EnableExecuting = 4,
    EnableSucceeded = 5,
    EnableFailed = 6,
}

impl Phase {
    #[must_use]
    pub const fn from_bits(bits: u8) -> Option<Self> {
        match bits {
            0 => Some(Self::Idle),
            1 => Some(Self::ClearPreparing),
            2 => Some(Self::ClearFinalizing),
            3 => Some(Self::EnableQueued),
            4 => Some(Self::EnableExecuting),
            5 => Some(Self::EnableSucceeded),
            6 => Some(Self::EnableFailed),
            _ => None,
        }
    }

    #[must_use]
    pub const fn bits(self) -> u8 {
        self as u8
    }
}

/// 串口普通 IO 的六类抑制原因；位值与 `hard_rt_ctrl::inhibit` 一致。
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum InhibitReason {
    CommFault = 1,
    AdapterFault = 1 << 1,
    StopFailed = 1 << 2,
    Converging = 1 << 3,
    FaultPending = 1 << 4,
    Boot = 1 << 5,
}

impl InhibitReason {
    pub const ALL: [Self; 6] = [
        Self::CommFault,
        Self::AdapterFault,
        Self::StopFailed,
        Self::Converging,
        Self::FaultPending,
        Self::Boot,
    ];

    #[must_use]
    pub const fn bit(self) -> u8 {
        self as u8
    }

    /// 仅 Boot / Converging / FaultPending 可解除；其余沿本候选保持 sticky。
    #[must_use]
    pub const fn is_releasable(self) -> bool {
        matches!(self, Self::Boot | Self::Converging | Self::FaultPending)
    }
}

/// 抑制原因集合（6 位）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct InhibitSet(u8);

impl InhibitSet {
    pub const EMPTY: Self = Self(0);
    pub const ALL: Self = Self(0x3f);

    /// 拒绝未知位。
    #[must_use]
    pub const fn from_bits(bits: u8) -> Option<Self> {
        if bits & !0x3f == 0 {
            Some(Self(bits))
        } else {
            None
        }
    }

    const fn from_bits_truncate(bits: u8) -> Self {
        Self(bits & 0x3f)
    }

    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    #[must_use]
    pub const fn contains(self, reason: InhibitReason) -> bool {
        self.0 & reason.bit() != 0
    }

    #[must_use]
    pub const fn with(self, reason: InhibitReason) -> Self {
        Self(self.0 | reason.bit())
    }

    #[must_use]
    pub const fn without(self, reason: InhibitReason) -> Self {
        Self(self.0 & !reason.bit())
    }

    pub fn reasons(self) -> impl Iterator<Item = InhibitReason> {
        InhibitReason::ALL
            .into_iter()
            .filter(move |reason| self.contains(*reason))
    }
}

/// 构造参数。初始 epoch = 1、Disabled = true、Phase = Idle 固定，其余由此给定。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuthorityInit {
    pub latch: bool,
    pub safety: OutputSafetyState,
    pub inhibits: InhibitSet,
}

impl AuthorityInit {
    #[must_use]
    pub const fn new(latch: bool, safety: OutputSafetyState, inhibits: InhibitSet) -> Self {
        Self {
            latch,
            safety,
            inhibits,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthorityInitError {
    /// 全局 domain 计数耗尽；不回绕。
    DomainExhausted,
    /// 指定的初始权限 epoch 不在 `1..=MAX`。
    InvalidPermissionEpoch(u64),
}

/// 只读 `Copy` 快照；不能构造任何 owner 或执行许可。
///
/// ```compile_fail
/// use robot_s0::control_authority::{AuthoritySnapshot, EnableOwner};
/// fn forge(snapshot: AuthoritySnapshot) -> EnableOwner {
///     EnableOwner { domain: snapshot.domain, epoch: snapshot.epoch.get() }
/// }
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "快照逐位镜像权限字，各布尔位语义独立"
)]
pub struct AuthoritySnapshot {
    pub domain: DomainId,
    pub epoch: PermissionEpoch,
    pub safety: OutputSafetyState,
    /// `None` 仅当编码为非法值 7；本模块从不产生。
    pub phase: Option<Phase>,
    pub inhibits: InhibitSet,
    pub latch: bool,
    pub disabled: bool,
    pub shutdown: bool,
    pub exhausted: bool,
    pub serial_estop_pending: bool,
    pub ipc_estop_notice: bool,
    pub normal_io_active: bool,
}

// ============================================================================
// 拒绝原因
// ============================================================================

/// 所有拒绝原因。每个入口按文档顺序判定，返回首个命中的原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// 令牌来自其它 domain；先拒且不改任何位。
    CrossDomain {
        token: DomainId,
        authority: DomainId,
    },
    /// Phase 编码为非法值 7；本模块从不产生，出现即 fail-closed。
    CorruptedPhase,
    /// 需要 Idle，但已有事务占位。
    Busy(Phase),
    /// 当前 Phase 不是该操作要求的阶段。
    WrongPhase(Phase),
    /// 令牌绑定的权限 epoch 已被撤销。
    StaleEpoch {
        held: PermissionEpoch,
        current: PermissionEpoch,
    },
    Latched,
    Shutdown,
    Exhausted,
    Inhibited(InhibitSet),
    /// 需要 Disabled = true。
    NotDisabled,
    /// 需要 Disabled = false。
    Disabled,
    SerialEStopPending,
    NormalIoActive,
    SafetyNotOffConfirmed(OutputSafetyState),
    SafetyNotTorqueOn(OutputSafetyState),
    /// StopFailed / AdapterUncertain 不可被放宽。
    SafetyNotRelaxable(OutputSafetyState),
    /// 下一权限 epoch 不存在；权威已置 Exhausted + Disabled。
    EpochExhausted,
    /// 观测采样的四字段之一已变化。
    ObservationStale(ObservedField),
    /// 当前所需文件目标与修复许可不符，须用同一 owner 重新修复。
    RepairTargetChanged {
        required: RepairTarget,
    },
    /// Finalizing 且 Latch = false：应走 Finalize，而不是修复。
    RepairNotNeeded,
    InhibitNotSet(InhibitReason),
    InhibitSticky(InhibitReason),
    /// 释放时 `NORMAL_IO_ACTIVE` 并未置位。
    LeaseNotActive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObservedField {
    Epoch,
    Phase,
    Safety,
}

/// 拒绝时归还的令牌及原因；令牌仍属于原事务，清理责任不丢失。
#[derive(Debug)]
pub struct Refused<T> {
    pub token: T,
    pub reason: Refusal,
}

// ============================================================================
// 收紧结果
// ============================================================================

/// 一次撤销事件的记账。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tightened {
    /// 事件之后的权限 epoch。
    pub epoch: PermissionEpoch,
    /// 是否真正推进了 epoch；到达 MAX 后为 `false`。
    pub advanced: bool,
    /// 事件之后是否处于 Exhausted。
    pub exhausted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EStopTightened {
    pub outcome: EStopOutcome,
    pub tightened: Tightened,
    /// EStop 线性化时普通 IO 是否在途；在途事务沿既有安全责任退场。
    pub normal_io_active: bool,
}

/// 有限的 typed 安全收紧；没有接受任意状态的 setter。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SafetyTightening {
    StopCommanded,
    StopFailed,
    AdapterUncertain,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SafetyTightened {
    pub previous: OutputSafetyState,
    pub safety: OutputSafetyState,
    pub tightened: Tightened,
}

// ============================================================================
// Enable 令牌
// ============================================================================

/// Enable 事务的唯一 owner；非 `Clone`，按值消费。
///
/// ```compile_fail
/// use robot_s0::control_authority::EnableOwner;
/// fn duplicate(owner: EnableOwner) { let _ = owner.clone(); }
/// ```
///
/// ```compile_fail
/// use robot_s0::control_authority::{ControlAuthority, EnableOwner};
/// fn retire_twice(authority: &ControlAuthority, owner: EnableOwner) {
///     let _ = authority.retire_enable(owner);
///     let _ = authority.retire_enable(owner);
/// }
/// ```
#[derive(Debug)]
pub struct EnableOwner {
    domain: DomainId,
    epoch: u64,
}

impl EnableOwner {
    #[must_use]
    pub const fn domain(&self) -> DomainId {
        self.domain
    }

    #[must_use]
    pub const fn epoch(&self) -> PermissionEpoch {
        PermissionEpoch(self.epoch)
    }
}

/// 只读 `Copy` 启动视图。复制它不能重启事务：`Queued → Executing` 只能成功一次。
///
/// 视图不能提交或退休事务：
///
/// ```compile_fail
/// use robot_s0::control_authority::{ControlAuthority, EnableStartView};
/// fn commit_with_view(authority: &ControlAuthority, view: EnableStartView) {
///     let _ = authority.commit_enable(view);
/// }
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EnableStartView {
    domain: DomainId,
    epoch: u64,
}

impl EnableStartView {
    #[must_use]
    pub const fn domain(&self) -> DomainId {
        self.domain
    }

    #[must_use]
    pub const fn epoch(&self) -> PermissionEpoch {
        PermissionEpoch(self.epoch)
    }
}

/// 唯一执行令牌；覆盖整个真实 adapter 动作（含多个 wire 事务）。
///
/// ```compile_fail
/// use robot_s0::control_authority::EnableExecution;
/// fn duplicate(execution: EnableExecution) { let _ = execution.clone(); }
/// ```
#[derive(Debug)]
pub struct EnableExecution {
    domain: DomainId,
    epoch: u64,
}

impl EnableExecution {
    #[must_use]
    pub const fn domain(&self) -> DomainId {
        self.domain
    }

    #[must_use]
    pub const fn epoch(&self) -> PermissionEpoch {
        PermissionEpoch(self.epoch)
    }

    /// 派生只读视图供各 wire 步骤经同域 IO acquire 核验；一次 wire release 不等于
    /// 整个 Enable 完成。
    #[must_use]
    pub const fn view(&self) -> EnableExecutionView {
        EnableExecutionView {
            domain: self.domain,
            epoch: self.epoch,
        }
    }
}

/// 执行视图；只能用于 [`NormalIoIntent::EnableStep`]，不能完成或退休事务。
///
/// ```compile_fail
/// use robot_s0::control_authority::{ControlAuthority, EnableExecutionView, EnableResult};
/// fn finish_with_view(authority: &ControlAuthority, view: EnableExecutionView) {
///     let _ = authority.finish_enable(view, EnableResult::Succeeded);
/// }
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EnableExecutionView {
    domain: DomainId,
    epoch: u64,
}

impl EnableExecutionView {
    #[must_use]
    pub const fn domain(&self) -> DomainId {
        self.domain
    }

    #[must_use]
    pub const fn epoch(&self) -> PermissionEpoch {
        PermissionEpoch(self.epoch)
    }
}

/// 真实 adapter 动作的结果。失败或不可信的安全结果另走 [`ControlAuthority::tighten_safety`]。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnableResult {
    Succeeded,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EnableFinish {
    /// `EnableSucceeded` 或 `EnableFailed`。
    pub phase: Phase,
    /// 只有原 epoch / 恢复门仍有效时，真实成功才发布 TorqueOn。
    pub torque_on_published: bool,
    /// 事务权限在完成前已被撤销（epoch 改变或 Exhausted）。
    pub superseded: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EnableCommitted {
    /// 提交之后的权限 epoch。
    pub epoch: PermissionEpoch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EnableRetired {
    pub retired_from: Phase,
    pub superseded: bool,
}

// ============================================================================
// Clear 令牌
// ============================================================================

/// Clear 事务 Preparing 阶段的唯一 owner。
///
/// ```compile_fail
/// use robot_s0::control_authority::ClearPendingOwner;
/// fn duplicate(owner: ClearPendingOwner) { let _ = owner.clone(); }
/// ```
#[derive(Debug)]
pub struct ClearPendingOwner {
    domain: DomainId,
    epoch: u64,
    original_latched: bool,
}

impl ClearPendingOwner {
    #[must_use]
    pub const fn domain(&self) -> DomainId {
        self.domain
    }

    /// 本事务的权限 epoch，即 A1 marker 应记录的 `expected_epoch`。
    #[must_use]
    pub const fn epoch(&self) -> PermissionEpoch {
        PermissionEpoch(self.epoch)
    }

    /// 事务开始时的 Latch，即 A1 marker 应记录的 `original_latched`。
    #[must_use]
    pub const fn original_latched(&self) -> bool {
        self.original_latched
    }
}

/// Clear 已逻辑提交（Latch 已清），等待文件收尾的唯一 owner。
///
/// ```compile_fail
/// use robot_s0::control_authority::ClearFinalizingOwner;
/// fn duplicate(owner: ClearFinalizingOwner) { let _ = owner.clone(); }
/// ```
#[derive(Debug)]
pub struct ClearFinalizingOwner {
    domain: DomainId,
    epoch: u64,
    original_latched: bool,
}

impl ClearFinalizingOwner {
    #[must_use]
    pub const fn domain(&self) -> DomainId {
        self.domain
    }

    /// 提交之后的权限 epoch。
    #[must_use]
    pub const fn epoch(&self) -> PermissionEpoch {
        PermissionEpoch(self.epoch)
    }

    #[must_use]
    pub const fn original_latched(&self) -> bool {
        self.original_latched
    }
}

/// 修复入口接受的两种 owner；本身非 `Clone`。
#[derive(Debug)]
pub enum ClearOwner {
    Pending(ClearPendingOwner),
    Finalizing(ClearFinalizingOwner),
}

impl ClearOwner {
    #[must_use]
    pub const fn domain(&self) -> DomainId {
        match self {
            Self::Pending(owner) => owner.domain,
            Self::Finalizing(owner) => owner.domain,
        }
    }

    /// owner 自己的权限 epoch。
    #[must_use]
    pub const fn epoch(&self) -> PermissionEpoch {
        match self {
            Self::Pending(owner) => PermissionEpoch(owner.epoch),
            Self::Finalizing(owner) => PermissionEpoch(owner.epoch),
        }
    }

    #[must_use]
    pub const fn original_latched(&self) -> bool {
        match self {
            Self::Pending(owner) => owner.original_latched,
            Self::Finalizing(owner) => owner.original_latched,
        }
    }

    const fn expected_phase(&self) -> Phase {
        match self {
            Self::Pending(_) => Phase::ClearPreparing,
            Self::Finalizing(_) => Phase::ClearFinalizing,
        }
    }

    const fn logical_committed(&self) -> bool {
        matches!(self, Self::Finalizing(_))
    }
}

/// 文件修复目标。没有 `Original(true)`：原 latch 为 true 时目标就是 `Latched`，
/// 已提交的 Clear 也不得被回滚为 true。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepairTarget {
    /// 双份写 `1`。
    Latched,
    /// 事务开始前 Latch 为 false 且当前仍未锁存：恢复原 `0`。
    OriginalUnlatched,
}

/// 修复许可：内含唯一 owner、目标与采样 epoch，不另返可与 owner 错配的 decision。
///
/// ```compile_fail
/// use robot_s0::control_authority::RepairPermit;
/// fn duplicate(permit: RepairPermit) { let _ = permit.clone(); }
/// ```
#[derive(Debug)]
pub struct RepairPermit {
    owner: ClearOwner,
    target: RepairTarget,
    sampled_epoch: u64,
}

impl RepairPermit {
    #[must_use]
    pub const fn target(&self) -> RepairTarget {
        self.target
    }

    #[must_use]
    pub const fn sampled_epoch(&self) -> PermissionEpoch {
        PermissionEpoch(self.sampled_epoch)
    }

    #[must_use]
    pub const fn owner(&self) -> &ClearOwner {
        &self.owner
    }
}

/// 清理许可的种类。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CleanupKind {
    /// 已提交 Clear 的文件收尾：删除 marker，保留双 `0`。
    Finalize,
    /// 修复目标已落盘后删除 marker。
    Repaired(RepairTarget),
}

/// 文件清理许可；只是调用方的文件义务，不代表 marker 已删除。
///
/// ```compile_fail
/// use robot_s0::control_authority::CleanupPermit;
/// fn duplicate(permit: CleanupPermit) { let _ = permit.clone(); }
/// ```
#[derive(Debug)]
pub struct CleanupPermit {
    owner: ClearOwner,
    kind: CleanupKind,
    sampled_epoch: u64,
}

impl CleanupPermit {
    #[must_use]
    pub const fn kind(&self) -> CleanupKind {
        self.kind
    }

    #[must_use]
    pub const fn sampled_epoch(&self) -> PermissionEpoch {
        PermissionEpoch(self.sampled_epoch)
    }

    #[must_use]
    pub const fn owner(&self) -> &ClearOwner {
        &self.owner
    }
}

/// Clear 事务退休记账。`logical_committed` 与 `superseded` 分别记录：
/// 后到的 Disable / EStop 不能把已发生的逻辑提交误报成"Clear 从未发生"。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClearRetirement {
    pub kind: CleanupKind,
    /// `commit_clear` 曾成功（Latch 曾被本事务清除）。
    pub logical_committed: bool,
    /// 退休时权限 epoch 已不同于 owner 自己的 epoch，或权威已 Exhausted。
    pub superseded: bool,
    /// 退休时的 Latch。
    pub latch: bool,
}

// ============================================================================
// inhibit 解除令牌
// ============================================================================

/// 单原因解除许可；过期后必须重走捕获与真实义务复核。
///
/// ```compile_fail
/// use robot_s0::control_authority::InhibitReleasePermit;
/// fn duplicate(permit: InhibitReleasePermit) { let _ = permit.clone(); }
/// ```
#[derive(Debug)]
pub struct InhibitReleasePermit {
    domain: DomainId,
    epoch: u64,
    reason: InhibitReason,
}

impl InhibitReleasePermit {
    #[must_use]
    pub const fn reason(&self) -> InhibitReason {
        self.reason
    }

    #[must_use]
    pub const fn epoch(&self) -> PermissionEpoch {
        PermissionEpoch(self.epoch)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InhibitReleased {
    pub reason: InhibitReason,
    /// 解除之后的权限 epoch。
    pub epoch: PermissionEpoch,
    pub remaining: InhibitSet,
}

/// 解除失败：跨 domain 归还许可；其它原因消费许可，不能自动刷 epoch 重试。
#[derive(Debug)]
pub enum InhibitReleaseError {
    CrossDomain(InhibitReleasePermit),
    Expired(Refusal),
}

// ============================================================================
// 安全观测令牌
// ============================================================================

/// 闭合的安全操作集合。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SafetyOperation {
    ConfirmOff,
    Disable,
    Disconnect,
}

/// 真实操作开始时采样的私有上下文（domain / epoch / Phase / safety）；非 `Clone`。
///
/// ```compile_fail
/// use robot_s0::control_authority::SafetyObservation;
/// fn duplicate(observation: SafetyObservation) { let _ = observation.clone(); }
/// ```
#[derive(Debug)]
pub struct SafetyObservation {
    domain: DomainId,
    epoch: u64,
    phase_bits: u8,
    safety: OutputSafetyState,
    operation: SafetyOperation,
}

impl SafetyObservation {
    #[must_use]
    pub const fn operation(&self) -> SafetyOperation {
        self.operation
    }

    #[must_use]
    pub const fn epoch(&self) -> PermissionEpoch {
        PermissionEpoch(self.epoch)
    }

    #[must_use]
    pub const fn safety(&self) -> OutputSafetyState {
        self.safety
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SafetyObservationCommitted {
    pub operation: SafetyOperation,
    pub previous: OutputSafetyState,
    pub safety: OutputSafetyState,
    /// Disconnect 撤销更早观测时的记账；ConfirmOff / Disable 不推进 epoch。
    pub tightened: Option<Tightened>,
}

/// 观测失败：跨 domain 归还令牌；其它拒绝消费令牌，过期结果不能复述。
#[derive(Debug)]
pub enum SafetyObservationError {
    CrossDomain(SafetyObservation),
    Rejected(Refusal),
}

// ============================================================================
// 普通 IO 租约
// ============================================================================

/// 非上扭控制的闭合枚举；没有未知 `NormalEffectful` 通配豁免。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NonEnergizingControl {
    Connect,
    Disconnect,
    Disable,
}

/// 普通 IO 意图。基础门（无 serial pending、无 inhibit、IO 空闲）对所有意图生效。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NormalIoIntent {
    /// 健康读取；不因 Latch / Disabled / Clear 阶段拒绝。
    PeriodicRead,
    /// 周期写及模式控制（含所有 SetMode）；须未 Latch、Idle、未 Disabled、未 Shutdown / Exhausted。
    Motion,
    /// 已授权 Enable 的一个 wire 步骤；不因 Disabled 挡住。
    EnableStep(EnableExecutionView),
    /// Connect / Disconnect / Disable：只保留基础门。
    NonEnergizingControl(NonEnergizingControl),
}

/// 普通 IO 租约；唯一释放出口消费它，只清 `NORMAL_IO_ACTIVE`。
///
/// ```compile_fail
/// use robot_s0::control_authority::NormalIoLease;
/// fn duplicate(lease: NormalIoLease) { let _ = lease.clone(); }
/// ```
#[derive(Debug)]
pub struct NormalIoLease {
    domain: DomainId,
    epoch: u64,
    intent: NormalIoIntent,
}

impl NormalIoLease {
    #[must_use]
    pub const fn intent(&self) -> NormalIoIntent {
        self.intent
    }

    /// 获取时的权限 epoch；跨 epoch 仍可释放，但不代表运动权限延续。
    #[must_use]
    pub const fn epoch(&self) -> PermissionEpoch {
        PermissionEpoch(self.epoch)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NormalIoReleased {
    pub intent: NormalIoIntent,
    /// 获取到释放之间权限已变化（epoch 改变或 Exhausted）。
    pub superseded: bool,
}

// ============================================================================
// ControlAuthority
// ============================================================================

enum Decision<T> {
    Commit(Word, T),
    Keep(T),
}

/// 测试专用：每次 CAS 之前注入的交错动作。
#[cfg(test)]
type BeforeCasHook = Box<dyn FnMut(&ControlAuthority) + Send>;

/// 单个权威域：一个 `AtomicU64` 与初始化时取得的只读 domain。
///
/// 不公开原子、不提供 raw setter；非 `Clone`。跨线程共享由调用方以 `Arc`
/// 在非 RT 路径完成一次。
pub struct ControlAuthority {
    word: AtomicU64,
    domain: DomainId,
    #[cfg(test)]
    before_cas: std::sync::Mutex<Option<BeforeCasHook>>,
}

impl std::fmt::Debug for ControlAuthority {
    /// 测试 hook 不参与状态语义，故不打印；快照已覆盖权限字全部字段。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlAuthority")
            .field("domain", &self.domain)
            .field("word", &self.word.load(Ordering::Acquire))
            .field("snapshot", &self.snapshot())
            .finish_non_exhaustive()
    }
}

fn allocate_domain(counter: &AtomicU64) -> Option<DomainId> {
    let mut current = counter.load(Ordering::Acquire);
    loop {
        let id = NonZeroU64::new(current)?;
        let next = current.checked_add(1)?;
        match counter.compare_exchange(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return Some(DomainId(id)),
            Err(actual) => current = actual,
        }
    }
}

/// Enable 恢复门：Exhausted / Shutdown / Latched / 必须 Disabled / 抑制 / 故障安全态。
/// Enable 事务全程要求 Disabled = true；已提交 Enable 之后的幂等返回由高层处理，
/// 不再占用底层使能事务。
fn enable_gate(word: Word) -> Option<Refusal> {
    if word.has(EXHAUSTED) {
        return Some(Refusal::Exhausted);
    }
    if word.has(SHUTDOWN) {
        return Some(Refusal::Shutdown);
    }
    if word.has(LATCH) {
        return Some(Refusal::Latched);
    }
    if !word.has(DISABLED) {
        return Some(Refusal::NotDisabled);
    }
    let inhibits = word.inhibits();
    if !inhibits.is_empty() {
        return Some(Refusal::Inhibited(inhibits));
    }
    match word.safety() {
        OutputSafetyState::TorqueUnknown
        | OutputSafetyState::TorqueOn
        | OutputSafetyState::StopCommanded
        | OutputSafetyState::TorqueOffConfirmed => None,
        state @ (OutputSafetyState::StopFailed | OutputSafetyState::AdapterUncertain) => {
            Some(Refusal::SafetyNotRelaxable(state))
        }
    }
}

/// Clear 恢复门：Exhausted / Shutdown / 必须 Disabled / 抑制 / 必须 TorqueOffConfirmed。
fn clear_gate(word: Word) -> Option<Refusal> {
    if word.has(EXHAUSTED) {
        return Some(Refusal::Exhausted);
    }
    if word.has(SHUTDOWN) {
        return Some(Refusal::Shutdown);
    }
    if !word.has(DISABLED) {
        return Some(Refusal::NotDisabled);
    }
    let inhibits = word.inhibits();
    if !inhibits.is_empty() {
        return Some(Refusal::Inhibited(inhibits));
    }
    match word.safety() {
        OutputSafetyState::TorqueOffConfirmed => None,
        state @ (OutputSafetyState::TorqueUnknown
        | OutputSafetyState::TorqueOn
        | OutputSafetyState::StopCommanded
        | OutputSafetyState::StopFailed
        | OutputSafetyState::AdapterUncertain) => Some(Refusal::SafetyNotOffConfirmed(state)),
    }
}

/// 当前所需的修复目标；Finalizing 且未锁存时不需要修复。
fn required_repair_target(owner: &ClearOwner, word: Word) -> Option<RepairTarget> {
    match owner {
        ClearOwner::Pending(pending) => {
            if pending.original_latched || word.has(LATCH) {
                Some(RepairTarget::Latched)
            } else {
                Some(RepairTarget::OriginalUnlatched)
            }
        }
        ClearOwner::Finalizing(_) => word.has(LATCH).then_some(RepairTarget::Latched),
    }
}

const fn tightened(word: Word, advance: Advance) -> Tightened {
    Tightened {
        epoch: PermissionEpoch(word.epoch()),
        advanced: advance.advanced,
        exhausted: advance.exhausted,
    }
}

impl ControlAuthority {
    /// 非 RT 构造：从全局计数分配 domain；初始 epoch = 1、Disabled、Idle。
    pub fn new(init: AuthorityInit) -> Result<Self, AuthorityInitError> {
        Self::with_counter(&NEXT_DOMAIN, init, PermissionEpoch::INITIAL)
    }

    /// 指定初始权限 epoch 的构造入口，仅供模块测试与 `test-comm-fault-hook`
    /// 验收夹具逼近 `MAX`；生产组合不编译此入口，默认构造固定 epoch = 1。
    /// 构造时不存在任何令牌或 owner，所以起始 epoch 不会撤销或复活任何权限。
    #[cfg(any(test, feature = "test-comm-fault-hook"))]
    pub fn new_with_permission_epoch(
        init: AuthorityInit,
        epoch: PermissionEpoch,
    ) -> Result<Self, AuthorityInitError> {
        Self::with_counter(&NEXT_DOMAIN, init, epoch)
    }

    fn with_counter(
        counter: &AtomicU64,
        init: AuthorityInit,
        epoch: PermissionEpoch,
    ) -> Result<Self, AuthorityInitError> {
        if PermissionEpoch::new(epoch.0).is_none() {
            return Err(AuthorityInitError::InvalidPermissionEpoch(epoch.0));
        }
        let domain = allocate_domain(counter).ok_or(AuthorityInitError::DomainExhausted)?;
        let mut word = Word::new(0)
            .with_safety(init.safety)
            .with_inhibits(init.inhibits)
            .with_phase(Phase::Idle)
            .with_epoch(epoch.0)
            .set(DISABLED);
        if init.latch {
            word = word.set(LATCH);
        }
        Ok(Self {
            word: AtomicU64::new(word.raw()),
            domain,
            #[cfg(test)]
            before_cas: std::sync::Mutex::new(None),
        })
    }

    #[must_use]
    pub const fn domain(&self) -> DomainId {
        self.domain
    }

    fn load(&self) -> Word {
        Word::new(self.word.load(Ordering::Acquire))
    }

    #[cfg(test)]
    fn run_before_cas(&self) {
        let hook = self.before_cas.lock().unwrap().take();
        if let Some(mut hook) = hook {
            hook(self);
            let mut slot = self.before_cas.lock().unwrap();
            if slot.is_none() {
                *slot = Some(hook);
            }
        }
    }

    /// 同域 CAS 循环：`decide` 基于最新观测值给出提交或放弃；提交失败按新值重判。
    fn update<T>(&self, mut decide: impl FnMut(Word) -> Decision<T>) -> T {
        let mut old = self.load();
        loop {
            match decide(old) {
                Decision::Keep(out) => return out,
                Decision::Commit(new, out) => {
                    #[cfg(test)]
                    self.run_before_cas();
                    match self.word.compare_exchange(
                        old.raw(),
                        new.raw(),
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        Ok(_) => return out,
                        Err(actual) => old = Word::new(actual),
                    }
                }
            }
        }
    }

    const fn check_domain(&self, token: DomainId) -> Result<(), Refusal> {
        if token.get() == self.domain.get() {
            Ok(())
        } else {
            Err(Refusal::CrossDomain {
                token,
                authority: self.domain,
            })
        }
    }

    /// 只读快照；不能构造任何 owner 或执行许可。
    #[must_use]
    pub fn snapshot(&self) -> AuthoritySnapshot {
        let word = self.load();
        AuthoritySnapshot {
            domain: self.domain,
            epoch: PermissionEpoch(word.epoch()),
            safety: word.safety(),
            phase: word.phase(),
            inhibits: word.inhibits(),
            latch: word.has(LATCH),
            disabled: word.has(DISABLED),
            shutdown: word.has(SHUTDOWN),
            exhausted: word.has(EXHAUSTED),
            serial_estop_pending: word.has(SERIAL_ESTOP_PENDING),
            ipc_estop_notice: word.has(IPC_ESTOP_NOTICE),
            normal_io_active: word.has(NORMAL_IO_ACTIVE),
        }
    }

    // ------------------------------------------------------------------
    // 安全撤销（永不失败）
    // ------------------------------------------------------------------

    /// EStop：置 Latch + Disabled + 双 notice，沿既有表收紧安全态，推进 epoch。
    /// 保留 Phase 与 `NORMAL_IO_ACTIVE`。
    #[must_use]
    pub fn estop(&self) -> EStopTightened {
        self.update(|old| {
            let previous = old.safety();
            let next = match previous {
                OutputSafetyState::TorqueUnknown
                | OutputSafetyState::TorqueOn
                | OutputSafetyState::TorqueOffConfirmed => OutputSafetyState::StopCommanded,
                OutputSafetyState::StopCommanded
                | OutputSafetyState::StopFailed
                | OutputSafetyState::AdapterUncertain => previous,
            };
            let outcome = match (old.has(SERIAL_ESTOP_PENDING), previous) {
                (true, _) | (false, OutputSafetyState::StopCommanded) => EStopOutcome::Merged,
                (false, OutputSafetyState::StopFailed | OutputSafetyState::AdapterUncertain) => {
                    EStopOutcome::LatchOnly
                }
                (
                    false,
                    OutputSafetyState::TorqueUnknown
                    | OutputSafetyState::TorqueOn
                    | OutputSafetyState::TorqueOffConfirmed,
                ) => EStopOutcome::StopIssued,
            };
            let (new, advance) = old
                .with_safety(next)
                .set(LATCH | DISABLED | SERIAL_ESTOP_PENDING | IPC_ESTOP_NOTICE)
                .advance_or_exhaust();
            Decision::Commit(
                new,
                EStopTightened {
                    outcome,
                    tightened: tightened(new, advance),
                    normal_io_active: old.has(NORMAL_IO_ACTIVE),
                },
            )
        })
    }

    /// Disable：只置 Disabled 并推进 epoch，不创造 EStop，不清 Phase / IO / notice。
    #[must_use]
    pub fn disable(&self) -> Tightened {
        self.update(|old| {
            let (new, advance) = old.set(DISABLED).advance_or_exhaust();
            Decision::Commit(new, tightened(new, advance))
        })
    }

    /// Shutdown：不可逆；置 Shutdown + Disabled 并推进 epoch。
    #[must_use]
    pub fn shutdown(&self) -> Tightened {
        self.update(|old| {
            let (new, advance) = old.set(SHUTDOWN | DISABLED).advance_or_exhaust();
            Decision::Commit(new, tightened(new, advance))
        })
    }

    /// 收紧一个抑制原因并推进 epoch；其它原因、Phase、保护位全部保留。
    #[must_use]
    pub fn tighten_inhibit(&self, reason: InhibitReason) -> Tightened {
        self.update(|old| {
            let (new, advance) = old
                .with_inhibits(old.inhibits().with(reason))
                .advance_or_exhaust();
            Decision::Commit(new, tightened(new, advance))
        })
    }

    /// 有限 typed 安全收紧：StopCommanded 不覆盖 Failed / Uncertain，Uncertain 不
    /// 覆盖 Failed，Failed 最保守；每次置 Disabled 并推进 epoch，饱和仍可收紧。
    #[must_use]
    pub fn tighten_safety(&self, tightening: SafetyTightening) -> SafetyTightened {
        self.update(|old| {
            let previous = old.safety();
            let next = match tightening {
                SafetyTightening::StopCommanded => match previous {
                    OutputSafetyState::TorqueUnknown
                    | OutputSafetyState::TorqueOn
                    | OutputSafetyState::TorqueOffConfirmed
                    | OutputSafetyState::StopCommanded => OutputSafetyState::StopCommanded,
                    OutputSafetyState::StopFailed | OutputSafetyState::AdapterUncertain => previous,
                },
                SafetyTightening::AdapterUncertain => match previous {
                    OutputSafetyState::StopFailed => OutputSafetyState::StopFailed,
                    OutputSafetyState::TorqueUnknown
                    | OutputSafetyState::TorqueOn
                    | OutputSafetyState::TorqueOffConfirmed
                    | OutputSafetyState::StopCommanded
                    | OutputSafetyState::AdapterUncertain => OutputSafetyState::AdapterUncertain,
                },
                SafetyTightening::StopFailed => OutputSafetyState::StopFailed,
            };
            let (new, advance) = old.with_safety(next).set(DISABLED).advance_or_exhaust();
            Decision::Commit(
                new,
                SafetyTightened {
                    previous,
                    safety: next,
                    tightened: tightened(new, advance),
                },
            )
        })
    }

    /// serial 消费 EStop pending；不推进 epoch。返回是否确有 pending。
    #[must_use]
    pub fn claim_serial_estop(&self) -> bool {
        self.update(|old| {
            if old.has(SERIAL_ESTOP_PENDING) {
                Decision::Commit(old.clear(SERIAL_ESTOP_PENDING), true)
            } else {
                Decision::Keep(false)
            }
        })
    }

    /// IPC 消费 EStop notice；不推进 epoch。
    #[must_use]
    pub fn claim_ipc_estop(&self) -> bool {
        self.update(|old| {
            if old.has(IPC_ESTOP_NOTICE) {
                Decision::Commit(old.clear(IPC_ESTOP_NOTICE), true)
            } else {
                Decision::Keep(false)
            }
        })
    }

    // ------------------------------------------------------------------
    // Enable 生命周期
    // ------------------------------------------------------------------

    /// Idle 且恢复门允许时推进 epoch、转 Queued，生成唯一 owner 与只读启动视图。
    pub fn reserve_enable(&self) -> Result<(EnableOwner, EnableStartView), Refusal> {
        let domain = self.domain;
        self.update(|old| {
            let Some(phase) = old.phase() else {
                return Decision::Keep(Err(Refusal::CorruptedPhase));
            };
            if phase != Phase::Idle {
                return Decision::Keep(Err(Refusal::Busy(phase)));
            }
            if let Some(refusal) = enable_gate(old) {
                return Decision::Keep(Err(refusal));
            }
            match old.next_epoch() {
                None => {
                    Decision::Commit(old.set(EXHAUSTED | DISABLED), Err(Refusal::EpochExhausted))
                }
                Some(epoch) => Decision::Commit(
                    old.with_epoch(epoch).with_phase(Phase::EnableQueued),
                    Ok((
                        EnableOwner { domain, epoch },
                        EnableStartView { domain, epoch },
                    )),
                ),
            }
        })
    }

    /// 唯一 `Queued → Executing` 转换；同一视图或其复制第二次启动均失败。
    pub fn start_enable(&self, view: EnableStartView) -> Result<EnableExecution, Refusal> {
        self.check_domain(view.domain)?;
        let domain = self.domain;
        self.update(|old| {
            let Some(phase) = old.phase() else {
                return Decision::Keep(Err(Refusal::CorruptedPhase));
            };
            if phase != Phase::EnableQueued {
                return Decision::Keep(Err(Refusal::WrongPhase(phase)));
            }
            if old.epoch() != view.epoch {
                return Decision::Keep(Err(Refusal::StaleEpoch {
                    held: PermissionEpoch(view.epoch),
                    current: PermissionEpoch(old.epoch()),
                }));
            }
            if let Some(refusal) = enable_gate(old) {
                return Decision::Keep(Err(refusal));
            }
            Decision::Commit(
                old.with_phase(Phase::EnableExecuting),
                Ok(EnableExecution {
                    domain,
                    epoch: view.epoch,
                }),
            )
        })
    }

    /// 整个真实 adapter 动作结束、其 IO 已释放后记录结果：同域核 `NORMAL_IO_ACTIVE`
    /// 为空，仍持有租约时拒绝并归还执行令牌。即使权限已撤销也记录真实结果，
    /// 但只有原 epoch / 恢复门仍有效时成功才发布 TorqueOn。
    pub fn finish_enable(
        &self,
        execution: EnableExecution,
        result: EnableResult,
    ) -> Result<EnableFinish, Refused<EnableExecution>> {
        if let Err(reason) = self.check_domain(execution.domain) {
            return Err(Refused {
                token: execution,
                reason,
            });
        }
        let held = execution.epoch;
        self.update(|old| {
            let Some(phase) = old.phase() else {
                return Decision::Keep(Err(Refusal::CorruptedPhase));
            };
            if phase != Phase::EnableExecuting {
                return Decision::Keep(Err(Refusal::WrongPhase(phase)));
            }
            if old.has(NORMAL_IO_ACTIVE) {
                return Decision::Keep(Err(Refusal::NormalIoActive));
            }
            let superseded = old.epoch() != held || old.has(EXHAUSTED);
            match result {
                EnableResult::Failed => Decision::Commit(
                    old.with_phase(Phase::EnableFailed),
                    Ok(EnableFinish {
                        phase: Phase::EnableFailed,
                        torque_on_published: false,
                        superseded,
                    }),
                ),
                EnableResult::Succeeded => {
                    let publish = !superseded && enable_gate(old).is_none();
                    let mut new = old.with_phase(Phase::EnableSucceeded);
                    if publish {
                        new = new.with_safety(OutputSafetyState::TorqueOn);
                    }
                    Decision::Commit(
                        new,
                        Ok(EnableFinish {
                            phase: Phase::EnableSucceeded,
                            torque_on_published: publish,
                            superseded,
                        }),
                    )
                }
            }
        })
        .map_err(|reason| Refused {
            token: execution,
            reason,
        })
    }

    /// 仅 Succeeded、当前 safety = TorqueOn 且原 epoch / 恢复门仍有效时消费 owner，
    /// 清 Disabled、回 Idle 并推进 epoch；迟到成功不能覆盖新保护。
    pub fn commit_enable(
        &self,
        owner: EnableOwner,
    ) -> Result<EnableCommitted, Refused<EnableOwner>> {
        if let Err(reason) = self.check_domain(owner.domain) {
            return Err(Refused {
                token: owner,
                reason,
            });
        }
        let held = owner.epoch;
        self.update(|old| {
            let Some(phase) = old.phase() else {
                return Decision::Keep(Err(Refusal::CorruptedPhase));
            };
            if phase != Phase::EnableSucceeded {
                return Decision::Keep(Err(Refusal::WrongPhase(phase)));
            }
            if old.epoch() != held {
                return Decision::Keep(Err(Refusal::StaleEpoch {
                    held: PermissionEpoch(held),
                    current: PermissionEpoch(old.epoch()),
                }));
            }
            if let Some(refusal) = enable_gate(old) {
                return Decision::Keep(Err(refusal));
            }
            let safety = old.safety();
            if safety != OutputSafetyState::TorqueOn {
                return Decision::Keep(Err(Refusal::SafetyNotTorqueOn(safety)));
            }
            match old.next_epoch() {
                None => {
                    Decision::Commit(old.set(EXHAUSTED | DISABLED), Err(Refusal::EpochExhausted))
                }
                Some(epoch) => Decision::Commit(
                    old.with_epoch(epoch)
                        .clear(DISABLED)
                        .with_phase(Phase::Idle),
                    Ok(EnableCommitted {
                        epoch: PermissionEpoch(epoch),
                    }),
                ),
            }
        })
        .map_err(|reason| Refused {
            token: owner,
            reason,
        })
    }

    /// 仅 Queued 或真实终态可消费 owner 回 Idle；Executing 拒绝并返还 owner。
    /// 只清本事务占位、保留最新保护，不推进 epoch。
    pub fn retire_enable(&self, owner: EnableOwner) -> Result<EnableRetired, Refused<EnableOwner>> {
        if let Err(reason) = self.check_domain(owner.domain) {
            return Err(Refused {
                token: owner,
                reason,
            });
        }
        let held = owner.epoch;
        self.update(|old| match old.phase() {
            None => Decision::Keep(Err(Refusal::CorruptedPhase)),
            Some(
                retired_from @ (Phase::EnableQueued | Phase::EnableSucceeded | Phase::EnableFailed),
            ) => Decision::Commit(
                old.with_phase(Phase::Idle),
                Ok(EnableRetired {
                    retired_from,
                    superseded: old.epoch() != held || old.has(EXHAUSTED),
                }),
            ),
            Some(
                phase @ (Phase::Idle
                | Phase::ClearPreparing
                | Phase::ClearFinalizing
                | Phase::EnableExecuting),
            ) => Decision::Keep(Err(Refusal::WrongPhase(phase))),
        })
        .map_err(|reason| Refused {
            token: owner,
            reason,
        })
    }

    // ------------------------------------------------------------------
    // Clear 生命周期
    // ------------------------------------------------------------------

    /// Disabled、Idle、TorqueOffConfirmed、无阻断原因且权限有效时推进 epoch、
    /// 转 Preparing，生成唯一 owner。A3 还必须核唯一 owner 维护的 Safe 与业务 guard。
    pub fn reserve_clear(&self) -> Result<ClearPendingOwner, Refusal> {
        let domain = self.domain;
        self.update(|old| {
            let Some(phase) = old.phase() else {
                return Decision::Keep(Err(Refusal::CorruptedPhase));
            };
            if phase != Phase::Idle {
                return Decision::Keep(Err(Refusal::Busy(phase)));
            }
            if let Some(refusal) = clear_gate(old) {
                return Decision::Keep(Err(refusal));
            }
            match old.next_epoch() {
                None => {
                    Decision::Commit(old.set(EXHAUSTED | DISABLED), Err(Refusal::EpochExhausted))
                }
                Some(epoch) => Decision::Commit(
                    old.with_epoch(epoch).with_phase(Phase::ClearPreparing),
                    Ok(ClearPendingOwner {
                        domain,
                        epoch,
                        original_latched: old.has(LATCH),
                    }),
                ),
            }
        })
    }

    /// 同域核 domain / epoch / 守门后清 Latch、`Preparing → Finalizing` 并推进 epoch；
    /// Disabled 保持。失败返还原 owner。
    pub fn commit_clear(
        &self,
        owner: ClearPendingOwner,
    ) -> Result<ClearFinalizingOwner, Refused<ClearPendingOwner>> {
        if let Err(reason) = self.check_domain(owner.domain) {
            return Err(Refused {
                token: owner,
                reason,
            });
        }
        let domain = self.domain;
        let held = owner.epoch;
        let original_latched = owner.original_latched;
        self.update(|old| {
            let Some(phase) = old.phase() else {
                return Decision::Keep(Err(Refusal::CorruptedPhase));
            };
            if phase != Phase::ClearPreparing {
                return Decision::Keep(Err(Refusal::WrongPhase(phase)));
            }
            if old.epoch() != held {
                return Decision::Keep(Err(Refusal::StaleEpoch {
                    held: PermissionEpoch(held),
                    current: PermissionEpoch(old.epoch()),
                }));
            }
            if let Some(refusal) = clear_gate(old) {
                return Decision::Keep(Err(refusal));
            }
            match old.next_epoch() {
                None => {
                    Decision::Commit(old.set(EXHAUSTED | DISABLED), Err(Refusal::EpochExhausted))
                }
                Some(epoch) => Decision::Commit(
                    old.with_epoch(epoch)
                        .clear(LATCH)
                        .with_phase(Phase::ClearFinalizing),
                    Ok(ClearFinalizingOwner {
                        domain,
                        epoch,
                        original_latched,
                    }),
                ),
            }
        })
        .map_err(|reason| Refused {
            token: owner,
            reason,
        })
    }

    /// 生成内含唯一 owner、当前所需目标及采样 epoch 的修复许可。pre-commit 保原
    /// latch：`original_latched || current_latch` 为 Latched，否则 OriginalUnlatched；
    /// post-commit 只有当前 Latch = true 才需要修复，且目标只能是 Latched。
    pub fn begin_repair(&self, owner: ClearOwner) -> Result<RepairPermit, Refused<ClearOwner>> {
        if let Err(reason) = self.check_domain(owner.domain()) {
            return Err(Refused {
                token: owner,
                reason,
            });
        }
        let word = self.load();
        let Some(phase) = word.phase() else {
            return Err(Refused {
                token: owner,
                reason: Refusal::CorruptedPhase,
            });
        };
        if phase != owner.expected_phase() {
            return Err(Refused {
                token: owner,
                reason: Refusal::WrongPhase(phase),
            });
        }
        match required_repair_target(&owner, word) {
            Some(target) => Ok(RepairPermit {
                owner,
                target,
                sampled_epoch: word.epoch(),
            }),
            None => Err(Refused {
                token: owner,
                reason: Refusal::RepairNotNeeded,
            }),
        }
    }

    /// 文件修复实际完成后：在同域 CAS 授权点核采样 epoch 未变且当前所需目标仍与
    /// 许可一致（无变化 CAS 线性化本次授权），才返回唯一清理许可；不匹配返还同一
    /// owner 供新一轮修复，占位与 marker 保留。饱和后 epoch 不再变化，所以目标
    /// 复核不能省略。
    pub fn authorize_repair_cleanup(
        &self,
        permit: RepairPermit,
    ) -> Result<CleanupPermit, Refused<ClearOwner>> {
        let RepairPermit {
            owner,
            target,
            sampled_epoch,
        } = permit;
        if let Err(reason) = self.check_domain(owner.domain()) {
            return Err(Refused {
                token: owner,
                reason,
            });
        }
        let expected_phase = owner.expected_phase();
        let decision = self.update(|old| {
            let Some(phase) = old.phase() else {
                return Decision::Keep(Err(Refusal::CorruptedPhase));
            };
            if phase != expected_phase {
                return Decision::Keep(Err(Refusal::WrongPhase(phase)));
            }
            if old.epoch() != sampled_epoch {
                return Decision::Keep(Err(Refusal::StaleEpoch {
                    held: PermissionEpoch(sampled_epoch),
                    current: PermissionEpoch(old.epoch()),
                }));
            }
            match required_repair_target(&owner, old) {
                Some(required) if required == target => Decision::Commit(old, Ok(())),
                Some(required) => Decision::Keep(Err(Refusal::RepairTargetChanged { required })),
                None => Decision::Keep(Err(Refusal::RepairNotNeeded)),
            }
        });
        match decision {
            Ok(()) => Ok(CleanupPermit {
                owner,
                kind: CleanupKind::Repaired(target),
                sampled_epoch,
            }),
            Err(reason) => Err(Refused {
                token: owner,
                reason,
            }),
        }
    }

    /// 双 `0` 已 prepared 且当前 Latch = false 时，在同域 CAS 授权点核当前保护状态
    /// 后消费 Finalizing owner，授予已提交 Clear 的文件收尾许可。后到 Disable 可以
    /// 使运动仍被禁止，但不要求回滚已提交的 Clear；不授运动权限。
    ///
    /// 授权 CAS 之后到达的 EStop 属于"原语先行、持久化随后"的边界：只有 marker
    /// 清理尚未开始且仍持有许可时，调用方才可经 [`Self::abandon_cleanup`] 取回
    /// owner 重新修复；清理期间或清理已 strict 完成后，权威保护（Latch / Disabled /
    /// epoch）已经生效，不能声称还能放弃未做的清理，后续持久化写 `1` 由 A3 的
    /// 唯一 writer 承担，实际清理成功后按真实结果调用
    /// [`Self::retire_clear_cleanup`] 退休并记 `superseded`。
    pub fn authorize_finalize(
        &self,
        owner: ClearFinalizingOwner,
    ) -> Result<CleanupPermit, Refused<ClearFinalizingOwner>> {
        if let Err(reason) = self.check_domain(owner.domain) {
            return Err(Refused {
                token: owner,
                reason,
            });
        }
        let decision = self.update(|old| {
            let Some(phase) = old.phase() else {
                return Decision::Keep(Err(Refusal::CorruptedPhase));
            };
            if phase != Phase::ClearFinalizing {
                return Decision::Keep(Err(Refusal::WrongPhase(phase)));
            }
            if old.has(LATCH) {
                return Decision::Keep(Err(Refusal::Latched));
            }
            Decision::Commit(old, Ok(old.epoch()))
        });
        match decision {
            Ok(sampled_epoch) => Ok(CleanupPermit {
                owner: ClearOwner::Finalizing(owner),
                kind: CleanupKind::Finalize,
                sampled_epoch,
            }),
            Err(reason) => Err(Refused {
                token: owner,
                reason,
            }),
        }
    }

    /// 文件清理尚未开始（或许可后到达新保护）时放弃许可，取回同一权限 owner 重新
    /// 修复或重新申请收尾。本入口只归还权限 owner、保持 Phase 占位，不改任何位；
    /// 它不检查也不重建文件，marker 是否仍存在由 A1 的实际文件结果判定（例如
    /// unlink 成功而目录同步失败时 marker 可能已不存在），"marker 保留"不是 A2
    /// 的保证。
    pub fn abandon_cleanup(
        &self,
        permit: CleanupPermit,
    ) -> Result<ClearOwner, Refused<CleanupPermit>> {
        if let Err(reason) = self.check_domain(permit.owner.domain()) {
            return Err(Refused {
                token: permit,
                reason,
            });
        }
        Ok(permit.owner)
    }

    /// 仅真实 marker 清理及 strict 目录同步成功后调用：消费许可、清自己的 Phase，
    /// 保留所有最新保护。`logical_committed` 与 `superseded` 分别记录。
    pub fn retire_clear_cleanup(
        &self,
        permit: CleanupPermit,
    ) -> Result<ClearRetirement, Refused<CleanupPermit>> {
        if let Err(reason) = self.check_domain(permit.owner.domain()) {
            return Err(Refused {
                token: permit,
                reason,
            });
        }
        let expected_phase = permit.owner.expected_phase();
        let owner_epoch = permit.owner.epoch().0;
        let logical_committed = permit.owner.logical_committed();
        let kind = permit.kind;
        self.update(|old| {
            let Some(phase) = old.phase() else {
                return Decision::Keep(Err(Refusal::CorruptedPhase));
            };
            if phase != expected_phase {
                return Decision::Keep(Err(Refusal::WrongPhase(phase)));
            }
            Decision::Commit(
                old.with_phase(Phase::Idle),
                Ok(ClearRetirement {
                    kind,
                    logical_committed,
                    superseded: old.epoch() != owner_epoch || old.has(EXHAUSTED),
                    latch: old.has(LATCH),
                }),
            )
        })
        .map_err(|reason| Refused {
            token: permit,
            reason,
        })
    }

    // ------------------------------------------------------------------
    // inhibit 解除
    // ------------------------------------------------------------------

    /// 捕获单原因解除许可：只对可解除原因、该位当前存在且未 Shutdown / Exhausted。
    pub fn capture_inhibit_release(
        &self,
        reason: InhibitReason,
    ) -> Result<InhibitReleasePermit, Refusal> {
        if !reason.is_releasable() {
            return Err(Refusal::InhibitSticky(reason));
        }
        let word = self.load();
        if word.has(EXHAUSTED) {
            return Err(Refusal::Exhausted);
        }
        if word.has(SHUTDOWN) {
            return Err(Refusal::Shutdown);
        }
        if !word.inhibits().contains(reason) {
            return Err(Refusal::InhibitNotSet(reason));
        }
        Ok(InhibitReleasePermit {
            domain: self.domain,
            epoch: word.epoch(),
            reason,
        })
    }

    /// 核 domain / epoch 及该位仍存在后只清该位并推进 epoch，保留其它原因、保护与
    /// Phase。许可过期即消费，必须重走捕获与真实义务复核。
    pub fn try_release_inhibit(
        &self,
        permit: InhibitReleasePermit,
    ) -> Result<InhibitReleased, InhibitReleaseError> {
        if self.check_domain(permit.domain).is_err() {
            return Err(InhibitReleaseError::CrossDomain(permit));
        }
        let held = permit.epoch;
        let reason = permit.reason;
        self.update(|old| {
            if old.has(EXHAUSTED) {
                return Decision::Keep(Err(Refusal::Exhausted));
            }
            if old.has(SHUTDOWN) {
                return Decision::Keep(Err(Refusal::Shutdown));
            }
            if old.epoch() != held {
                return Decision::Keep(Err(Refusal::StaleEpoch {
                    held: PermissionEpoch(held),
                    current: PermissionEpoch(old.epoch()),
                }));
            }
            let inhibits = old.inhibits();
            if !inhibits.contains(reason) {
                return Decision::Keep(Err(Refusal::InhibitNotSet(reason)));
            }
            match old.next_epoch() {
                None => {
                    Decision::Commit(old.set(EXHAUSTED | DISABLED), Err(Refusal::EpochExhausted))
                }
                Some(epoch) => {
                    let remaining = inhibits.without(reason);
                    Decision::Commit(
                        old.with_inhibits(remaining).with_epoch(epoch),
                        Ok(InhibitReleased {
                            reason,
                            epoch: PermissionEpoch(epoch),
                            remaining,
                        }),
                    )
                }
            }
        })
        .map_err(InhibitReleaseError::Expired)
    }

    // ------------------------------------------------------------------
    // 安全观测
    // ------------------------------------------------------------------

    /// 真实操作开始时采样 domain / epoch / Phase / safety。永不失败；结果能否
    /// 写回由完成时的复核决定。
    #[must_use]
    pub fn begin_safety_observation(&self, operation: SafetyOperation) -> SafetyObservation {
        let word = self.load();
        SafetyObservation {
            domain: self.domain,
            epoch: word.epoch(),
            phase_bits: word.phase_bits(),
            safety: word.safety(),
            operation,
        }
    }

    /// 仅在真实操作可信成功后调用；失败或不可信结果改走 [`Self::tighten_safety`]。
    /// 四字段仍匹配且非 Exhausted 才写回；notice / IO 位变化不要求相等。
    pub fn complete_safety_observation(
        &self,
        observation: SafetyObservation,
    ) -> Result<SafetyObservationCommitted, SafetyObservationError> {
        if self.check_domain(observation.domain).is_err() {
            return Err(SafetyObservationError::CrossDomain(observation));
        }
        let SafetyObservation {
            epoch,
            phase_bits,
            safety,
            operation,
            ..
        } = observation;
        self.update(|old| {
            if old.has(EXHAUSTED) {
                return Decision::Keep(Err(Refusal::Exhausted));
            }
            if old.phase().is_none() {
                return Decision::Keep(Err(Refusal::CorruptedPhase));
            }
            if old.epoch() != epoch {
                return Decision::Keep(Err(Refusal::ObservationStale(ObservedField::Epoch)));
            }
            if old.phase_bits() != phase_bits {
                return Decision::Keep(Err(Refusal::ObservationStale(ObservedField::Phase)));
            }
            if old.safety() != safety {
                return Decision::Keep(Err(Refusal::ObservationStale(ObservedField::Safety)));
            }
            match operation {
                SafetyOperation::ConfirmOff | SafetyOperation::Disable => {
                    if operation == SafetyOperation::Disable && !old.has(DISABLED) {
                        return Decision::Keep(Err(Refusal::NotDisabled));
                    }
                    match safety {
                        OutputSafetyState::TorqueUnknown
                        | OutputSafetyState::TorqueOn
                        | OutputSafetyState::StopCommanded
                        | OutputSafetyState::TorqueOffConfirmed => Decision::Commit(
                            old.with_safety(OutputSafetyState::TorqueOffConfirmed),
                            Ok(SafetyObservationCommitted {
                                operation,
                                previous: safety,
                                safety: OutputSafetyState::TorqueOffConfirmed,
                                tightened: None,
                            }),
                        ),
                        OutputSafetyState::StopFailed | OutputSafetyState::AdapterUncertain => {
                            Decision::Keep(Err(Refusal::SafetyNotRelaxable(safety)))
                        }
                    }
                }
                SafetyOperation::Disconnect => {
                    let next = match safety {
                        OutputSafetyState::TorqueUnknown
                        | OutputSafetyState::TorqueOn
                        | OutputSafetyState::StopCommanded
                        | OutputSafetyState::TorqueOffConfirmed => OutputSafetyState::TorqueUnknown,
                        OutputSafetyState::StopFailed | OutputSafetyState::AdapterUncertain => {
                            safety
                        }
                    };
                    let (new, advance) = old.with_safety(next).set(DISABLED).advance_or_exhaust();
                    Decision::Commit(
                        new,
                        Ok(SafetyObservationCommitted {
                            operation,
                            previous: safety,
                            safety: next,
                            tightened: Some(tightened(new, advance)),
                        }),
                    )
                }
            }
        })
        .map_err(SafetyObservationError::Rejected)
    }

    // ------------------------------------------------------------------
    // 普通 IO 租约
    // ------------------------------------------------------------------

    /// 基础门：无 serial pending、无 inhibit、IO 空闲；按意图追加门后 CAS 置
    /// `NORMAL_IO_ACTIVE`。Safety / Confirm / 启动 Connect 不走本租约。
    pub fn acquire_normal_io(&self, intent: NormalIoIntent) -> Result<NormalIoLease, Refusal> {
        if let NormalIoIntent::EnableStep(view) = intent {
            self.check_domain(view.domain)?;
        }
        let domain = self.domain;
        self.update(|old| {
            if old.has(SERIAL_ESTOP_PENDING) {
                return Decision::Keep(Err(Refusal::SerialEStopPending));
            }
            let inhibits = old.inhibits();
            if !inhibits.is_empty() {
                return Decision::Keep(Err(Refusal::Inhibited(inhibits)));
            }
            if old.has(NORMAL_IO_ACTIVE) {
                return Decision::Keep(Err(Refusal::NormalIoActive));
            }
            let Some(phase) = old.phase() else {
                return Decision::Keep(Err(Refusal::CorruptedPhase));
            };
            let extra = match intent {
                NormalIoIntent::PeriodicRead | NormalIoIntent::NonEnergizingControl(_) => None,
                NormalIoIntent::Motion => motion_gate(old, phase),
                NormalIoIntent::EnableStep(view) => enable_step_gate(old, phase, view.epoch),
            };
            if let Some(refusal) = extra {
                return Decision::Keep(Err(refusal));
            }
            Decision::Commit(
                old.set(NORMAL_IO_ACTIVE),
                Ok(NormalIoLease {
                    domain,
                    epoch: old.epoch(),
                    intent,
                }),
            )
        })
    }

    /// 唯一释放出口：消费租约，只清 `NORMAL_IO_ACTIVE`，保留一切新保护。
    pub fn release_normal_io(
        &self,
        lease: NormalIoLease,
    ) -> Result<NormalIoReleased, Refused<NormalIoLease>> {
        if let Err(reason) = self.check_domain(lease.domain) {
            return Err(Refused {
                token: lease,
                reason,
            });
        }
        let held = lease.epoch;
        let intent = lease.intent;
        self.update(|old| {
            if !old.has(NORMAL_IO_ACTIVE) {
                return Decision::Keep(Err(Refusal::LeaseNotActive));
            }
            Decision::Commit(
                old.clear(NORMAL_IO_ACTIVE),
                Ok(NormalIoReleased {
                    intent,
                    superseded: old.epoch() != held || old.has(EXHAUSTED),
                }),
            )
        })
        .map_err(|reason| Refused {
            token: lease,
            reason,
        })
    }
}

/// Motion 额外门：Exhausted / Shutdown / Latched / Disabled / 必须 Idle。
fn motion_gate(word: Word, phase: Phase) -> Option<Refusal> {
    if word.has(EXHAUSTED) {
        return Some(Refusal::Exhausted);
    }
    if word.has(SHUTDOWN) {
        return Some(Refusal::Shutdown);
    }
    if word.has(LATCH) {
        return Some(Refusal::Latched);
    }
    if word.has(DISABLED) {
        return Some(Refusal::Disabled);
    }
    if phase != Phase::Idle {
        return Some(Refusal::Busy(phase));
    }
    None
}

/// EnableStep 额外门：Exhausted / Shutdown / Latched / epoch 匹配 / 必须 Executing。
fn enable_step_gate(word: Word, phase: Phase, held: u64) -> Option<Refusal> {
    if word.has(EXHAUSTED) {
        return Some(Refusal::Exhausted);
    }
    if word.has(SHUTDOWN) {
        return Some(Refusal::Shutdown);
    }
    if word.has(LATCH) {
        return Some(Refusal::Latched);
    }
    if word.epoch() != held {
        return Some(Refusal::StaleEpoch {
            held: PermissionEpoch(held),
            current: PermissionEpoch(word.epoch()),
        });
    }
    if phase != Phase::EnableExecuting {
        return Some(Refusal::WrongPhase(phase));
    }
    None
}
