// Copyright (c) 2023 Contributors to the Eclipse Foundation
//
// See the NOTICE file(s) distributed with this work for additional
// information regarding copyright ownership.
//
// This program and the accompanying materials are made available under the
// terms of the Apache Software License 2.0 which is available at
// https://www.apache.org/licenses/LICENSE-2.0, or the MIT license
// which is available at https://opensource.org/licenses/MIT.
//
// SPDX-License-Identifier: Apache-2.0 OR MIT

pub mod common;
pub use common::details::checked_connection_management_size;
pub mod file;
pub mod posix_shared_memory;
pub mod process_local;
pub mod recommended;
pub mod used_chunk_list;

// RR R1（robot-system 本地补丁）：同源 channel 状态转移原语与私有测试。
pub(crate) mod channel_state;
#[cfg(test)]
mod tests;

use core::fmt::Debug;
use core::time::Duration;

pub use crate::shared_memory::PointerOffset;
use iceoryx2_bb_elementary_traits::testing::abandonable::Abandonable;
pub use iceoryx2_bb_system_types::file_name::*;
pub use iceoryx2_bb_system_types::path::Path;
use iceoryx2_log::fail;

use crate::static_storage::file::{NamedConcept, NamedConceptBuilder, NamedConceptMgmt};
use iceoryx2_bb_concurrency::atomic::{AtomicU64, Ordering};
use iceoryx2_bb_derive_macros::ZeroCopySend;
use iceoryx2_bb_elementary_traits::zero_copy_send::ZeroCopySend;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ZeroCopyPortRemoveError {
    InternalError,
    VersionMismatch,
    Interrupt,
    InsufficientPermissions,
    DoesNotExist,
}

impl core::fmt::Display for ZeroCopyPortRemoveError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "ZeroCopyPortRemoveError::{self:?}")
    }
}

impl core::error::Error for ZeroCopyPortRemoveError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ZeroCopyCreationError {
    InternalError,
    IsBeingCleanedUp,
    AnotherInstanceIsAlreadyConnected,
    InsufficientPermissions,
    VersionMismatch,
    ConnectionMaybeCorrupted,
    InvalidSampleSize,
    InitializationNotYetFinalized,
    IncompatibleBufferSize,
    IncompatibleMaxBorrowedSamplesPerChannelSetting,
    IncompatibleOverflowSetting,
    IncompatibleNumberOfSamples,
    IncompatibleNumberOfSegments,
    IncompatibleNumberOfChannels,
    /// RR R2：按 buffer / borrow / samples / segments / channels 受检计算管理区字节数时溢出，
    /// 或某个内部数组超出 `Layout::array` 表示界（在创建任何共享资源之前拒绝）。
    CapacityOverflow,
}

impl core::fmt::Display for ZeroCopyCreationError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}::{:?}", stringify!(Self), self)
    }
}

impl core::error::Error for ZeroCopyCreationError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZeroCopySendError {
    ConnectionCorrupted,
    ReceiveBufferFull,
    UnableToDeliver, // NOTE: in order to distinguish between a try_send failure and a user induced send aborting, the UnableToDeliver error is used
    UsedChunkListFull,
    NoConnectedReceiverAndBufferIsFull,
    ChannelIsClosed,
    InternalError,
}

impl core::fmt::Display for ZeroCopySendError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}::{:?}", stringify!(Self), self)
    }
}

impl core::error::Error for ZeroCopySendError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZeroCopyReceiveError {
    ReceiveWouldExceedMaxBorrowValue,
}

impl core::fmt::Display for ZeroCopyReceiveError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}::{:?}", stringify!(Self), self)
    }
}

impl core::error::Error for ZeroCopyReceiveError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZeroCopyReclaimError {
    ReceiverReturnedCorruptedPointerOffset,
}

impl core::fmt::Display for ZeroCopyReclaimError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}::{:?}", stringify!(Self), self)
    }
}

impl core::error::Error for ZeroCopyReclaimError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZeroCopyReleaseError {
    RetrieveBufferFull,
}

impl core::fmt::Display for ZeroCopyReleaseError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}::{:?}", stringify!(Self), self)
    }
}

impl core::error::Error for ZeroCopyReleaseError {}

/// RR R1：真实入队之后附带的问题。新 offset 已发布、新 used 已登记，问题只涉及旧 offset 的回收。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueIssue {
    /// 溢出挤出的旧 offset 不在 used 列表中：不能当作回收，也不回滚新引用。
    OverflowedOffsetNotInUsedList(PointerOffset),
}

/// RR R1：[`ZeroCopySender::try_send_observed()`] 的真实回执。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrySendReceipt {
    /// publication 之前拒绝：offset 未进 submission queue，used 集合未新增。
    NotEnqueued(ZeroCopySendError),
    /// 已 push 到 submission queue。`reclaimed_overflow` 仅在真实收到被挤出的旧 offset
    /// 且其 used 移除成功时为 `Some`；`issue` 记录 push 之后的旧件回收问题。
    Enqueued {
        reclaimed_overflow: Option<PointerOffset>,
        issue: Option<EnqueueIssue>,
    },
}

impl TrySendReceipt {
    /// 普通 [`ZeroCopySender::try_send()`] 的原 `Result` 投影：`NotEnqueued(e)` → `Err(e)`；
    /// 带 `issue` 的 `Enqueued` → `Err(ConnectionCorrupted)`（原行为，此时新 offset 仍已投递）；
    /// 其余 → `Ok(reclaimed_overflow)`。
    pub fn into_result(self) -> Result<Option<PointerOffset>, ZeroCopySendError> {
        match self {
            Self::NotEnqueued(error) => Err(error),
            Self::Enqueued { issue: Some(_), .. } => Err(ZeroCopySendError::ConnectionCorrupted),
            Self::Enqueued {
                reclaimed_overflow,
                issue: None,
            } => Ok(reclaimed_overflow),
        }
    }
}

/// Defines the action that shall be take when a [`PointerOffset`]
/// cannot be delivered.
#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub enum BackpressureToReceiverAction {
    /// Use an action which is derived from the `BackpressureStrategy`
    FollowBackpressureyStrategy,
    /// Retry to send and invoke the handler again, if sending does not succeed
    Retry,
    /// Discard the [`PointerOffset`] for the [`ZeroCopyReceiver`] which cause the incident
    /// and continue to deliver the [`PointerOffset`] to the remaining [`ZeroCopyReceiver`]s
    DiscardPointerOffset,
    /// Discard the [`PointerOffset`] for the [`ZeroCopyReceiver`] which caused the incident,
    /// continue to deliver the [`PointerOffset`] to the remaining [`ZeroCopyReceiver`]s;
    /// return with an error if the [`PointerOffset`] was not delivered to all [`ZeroCopyReceiver`]s
    DiscardPointerOffsetAndFail,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, ZeroCopySend)]
pub struct ChannelId(usize);

impl ChannelId {
    pub const fn new(value: usize) -> Self {
        Self(value)
    }

    pub const fn value(&self) -> usize {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelStateNewError {
    ValueOutOfBounds,
}

impl core::fmt::Display for ChannelStateNewError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}::{:?}", stringify!(Self), self)
    }
}

impl core::error::Error for ChannelStateNewError {}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, ZeroCopySend)]
pub struct ChannelState(u64);

impl ChannelState {
    pub fn new(value: u64) -> Result<Self, ChannelStateNewError> {
        if value > Self::max_value() {
            fail!(from "ChannelState::new()", with ChannelStateNewError::ValueOutOfBounds,
                "Unable to create new ChannelState since the value must be less or equal than 2^62 and this value is {value},");
        }

        Ok(Self(value))
    }

    pub const fn max_value() -> u64 {
        2u64.pow(62)
    }

    pub fn value(&self) -> u64 {
        self.0
    }
}

/// RR R1：strict 请求状态的公开三态。这只是 CAL 的观测原语：仅当调用方确实持有本次成功
/// strict prepare 的记录、原连接 route / pin，且该 channel 尚未复用时，`Closed` 才能作为该
/// 请求正常关闭的证据——同值 CLOSED 本身分不清初始与复用代次；无 prepare 资格或缺原 route 时
/// 调用方必须直接按 `Unknown` 处理。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrictChannelStatus {
    /// 仍是原请求（含 hint 形态）：尚未观察到终结，不声称对端存活。
    Open,
    /// 正常关闭（Acquire 读到 CLOSED）：关闭方 Release 之前入队的尾内容可见。
    Closed,
    /// ORPHANED、其它代次、非法输入或越界值：不能证明任何发布关系。
    Unknown,
}

/// RR R1：strict prepare 的失败原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrictPrepareError {
    /// 请求不在 strict 域 0..<2^62（普通上界 2^62 与 CLOSED 哨兵都在此拒绝）；未执行任何原子操作。
    IllegalRequest,
    /// 当前不是 CLOSED；携带相对本请求观察到的三态。
    NotClosed(StrictChannelStatus),
}

fn strict_status_of(observed: channel_state::Value, request: u64) -> StrictChannelStatus {
    match observed {
        channel_state::Value::Closed => StrictChannelStatus::Closed,
        channel_state::Value::Open(r) | channel_state::Value::Hinted(r) if r == request => {
            StrictChannelStatus::Open
        }
        _ => StrictChannelStatus::Unknown,
    }
}

pub const DEFAULT_BUFFER_SIZE: usize = 4;
pub const DEFAULT_ENABLE_SAFE_OVERFLOW: bool = false;
pub const DEFAULT_MAX_BORROWED_SAMPLES_PER_CHANNEL: usize = 4;
pub const DEFAULT_MAX_SUPPORTED_SHARED_MEMORY_SEGMENTS: u8 = 1;
pub const DEFAULT_NUMBER_OF_CHANNELS: usize = 1;
pub const DEFAULT_NUMBER_OF_SAMPLES_PER_SEGMENT: usize = 8;
/// The initial value of the channel state
pub const CHANNEL_STATE_OPEN: ChannelState = ChannelState(0);
/// A channel with an invalid state will never block in [`ZeroCopySender::blocking_send()`];
pub const CHANNEL_STATE_CLOSED: ChannelState = ChannelState(u64::MAX);
/// Hints the channel that the other side intends to disconnect.
const CHANNEL_STATE_DISCONNECT_HINT_BIT: u64 = 1u64 << 63;

// RR R1：生产侧适配。channel 状态字的真实类型是 iceoryx2_bb_concurrency 的 AtomicU64 包装，
// 直接为该类型实现同源 trait，两个方法都原样转发 Ordering。
impl channel_state::AtomicState for AtomicU64 {
    fn load(&self, order: Ordering) -> u64 {
        (**self).load(order)
    }

    fn compare_exchange(
        &self,
        current: u64,
        new: u64,
        success: Ordering,
        failure: Ordering,
    ) -> Result<u64, u64> {
        (**self).compare_exchange(current, new, success, failure)
    }
}

pub trait ZeroCopyConnectionBuilder<C: ZeroCopyConnection>: NamedConceptBuilder<C> {
    fn buffer_size(self, value: usize) -> Self;
    fn enable_safe_overflow(self, value: bool) -> Self;
    fn receiver_max_borrowed_chunks_per_channel(self, value: usize) -> Self;
    fn max_supported_shared_memory_segments(self, value: u8) -> Self;
    fn number_of_chunks_per_segment(self, value: usize) -> Self;
    fn number_of_channels(self, value: usize) -> Self;
    fn initial_channel_state(self, value: ChannelState) -> Self;
    /// The timeout defines how long the [`ZeroCopyConnectionBuilder`] should wait for
    /// concurrent
    /// [`ZeroCopyConnectionBuilder::create_sender()`] or
    /// [`ZeroCopyConnectionBuilder::create_receiver()`] call to finalize its initialization.
    /// By default it is set to [`Duration::ZERO`] for no timeout.
    fn timeout(self, value: Duration) -> Self;

    fn create_sender(self) -> Result<C::Sender, ZeroCopyCreationError>;
    fn create_receiver(self) -> Result<C::Receiver, ZeroCopyCreationError>;
}

pub trait ZeroCopyPortDetails {
    fn number_of_channels(&self) -> usize;
    fn buffer_size(&self) -> usize;
    fn has_enabled_safe_overflow(&self) -> bool;
    fn max_borrowed_chunks(&self) -> usize;
    fn max_supported_shared_memory_segments(&self) -> u8;
    fn is_connected(&self) -> bool;
    #[doc(hidden)]
    fn __internal_get_channel_state(&self, channel_id: ChannelId) -> &AtomicU64;

    /// CLOSED → `state`。合法请求走同源 `channel_state::prepare`（成功 AcqRel / 失败 Acquire，
    /// 成功条件与原来相同）；哨兵参数保持原 Relaxed CAS 的薄兼容边缘：
    /// `CHANNEL_STATE_CLOSED` 在 CLOSED 上仍是成功的 no-op，该 no-op 不构成 strict prepare 资格。
    fn set_channel_state(&self, channel_id: ChannelId, state: ChannelState) -> bool {
        let atomic = self.__internal_get_channel_state(channel_id);
        match channel_state::classify_value(state.0) {
            channel_state::Value::Open(request) => channel_state::prepare(atomic, request).is_ok(),
            _ => atomic
                .compare_exchange(
                    CHANNEL_STATE_CLOSED.0,
                    state.0,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
                .is_ok(),
        }
    }

    /// 合法请求走同源 `channel_state::set_hint`（只匹配原请求，不能从 ORPHANED / CLOSED 转换）；
    /// 哨兵参数保持原行为（`CHANNEL_STATE_CLOSED` 不改变值）。
    fn set_disconnect_hint(&self, channel_id: ChannelId, expected_state: ChannelState) {
        let atomic = self.__internal_get_channel_state(channel_id);
        match channel_state::classify_value(expected_state.0) {
            channel_state::Value::Open(request) => {
                let _ = channel_state::set_hint(atomic, request);
            }
            _ => {
                let disconnect_hint_state = expected_state.0 | CHANNEL_STATE_DISCONNECT_HINT_BIT;
                let _ = atomic.compare_exchange(
                    expected_state.0,
                    disconnect_hint_state,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                );
            }
        }
    }

    /// 合法请求走同源 `channel_state::has_hint`（Acquire）；哨兵参数保持原比较
    /// （`CHANNEL_STATE_CLOSED` 在 CLOSED 上为真）。
    fn has_disconnect_hint(&self, channel_id: ChannelId, expected_state: ChannelState) -> bool {
        let atomic = self.__internal_get_channel_state(channel_id);
        match channel_state::classify_value(expected_state.0) {
            channel_state::Value::Open(request) => channel_state::has_hint(atomic, request),
            _ => {
                let disconnect_hint_state = expected_state.0 | CHANNEL_STATE_DISCONNECT_HINT_BIT;
                disconnect_hint_state == atomic.load(Ordering::Relaxed)
            }
        }
    }

    /// 合法请求走同源 `channel_state::has_state`（Acquire，含 hint 形态）；哨兵参数保持原掩码
    /// 比较（`CHANNEL_STATE_CLOSED` 恒为假）。
    fn has_channel_state(&self, channel_id: ChannelId, expected_state: ChannelState) -> bool {
        let atomic = self.__internal_get_channel_state(channel_id);
        match channel_state::classify_value(expected_state.0) {
            channel_state::Value::Open(request) => channel_state::has_state(atomic, request),
            _ => {
                let state = atomic.load(Ordering::Relaxed);
                let state_without_disconnect_hint_bit =
                    state & !(CHANNEL_STATE_DISCONNECT_HINT_BIT);
                expected_state.0 == state_without_disconnect_hint_bit
            }
        }
    }

    /// 通用终止识别（Acquire）：CLOSED 与私有 ORPHANED 都视为已关闭（原实现只认 CLOSED）。
    /// strict 三态区分两者见 `channel_state::classify_prepared`。
    fn is_channel_closed(&self, channel_id: ChannelId) -> bool {
        channel_state::terminal(self.__internal_get_channel_state(channel_id))
    }

    /// 合法请求走同源 `channel_state::finish`（仅原请求 / 其 hint → CLOSED，成功 Release，
    /// 失败重新分类后不改其它代次或 ORPHANED）；哨兵参数保持原 Relaxed 序列
    /// （`CHANNEL_STATE_CLOSED` 不改变其它值）。
    fn close_channel(&self, channel_id: ChannelId, expected_state: ChannelState) {
        let atomic = self.__internal_get_channel_state(channel_id);
        match channel_state::classify_value(expected_state.0) {
            channel_state::Value::Open(request) => {
                let _ = channel_state::finish(atomic, request);
            }
            _ => match atomic.compare_exchange(
                expected_state.0,
                CHANNEL_STATE_CLOSED.0,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => (),
                Err(v) => {
                    let graceful_disconnect_state =
                        expected_state.0 | CHANNEL_STATE_DISCONNECT_HINT_BIT;
                    if v == graceful_disconnect_state {
                        let _ = atomic.compare_exchange(
                            graceful_disconnect_state,
                            CHANNEL_STATE_CLOSED.0,
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                        );
                    }
                }
            },
        }
    }

    /// RR R1 跨 crate 薄入口：strict prepare。仅 CLOSED → 受检 strict 请求（0..<2^62），
    /// 直接调用同源 `channel_state::prepare_strict`（成功 AcqRel / 失败 Acquire）。普通上界
    /// 2^62 与 CLOSED 哨兵在此拒绝且不触碰状态；返回只反映本次是否真 prepare 成功，不以通用
    /// `set_channel_state(CLOSED)` 的成功 no-op 代替。成功后的资格、原 route / pin 与未复用
    /// 由调用方保存。
    fn strict_prepare(
        &self,
        channel_id: ChannelId,
        request: ChannelState,
    ) -> Result<(), StrictPrepareError> {
        if !channel_state::is_strict_request(request.0) {
            return Err(StrictPrepareError::IllegalRequest);
        }
        match channel_state::prepare_strict(
            self.__internal_get_channel_state(channel_id),
            request.0,
        ) {
            Ok(()) => Ok(()),
            Err(channel_state::PrepareError::IllegalRequest) => {
                Err(StrictPrepareError::IllegalRequest)
            }
            Err(channel_state::PrepareError::NotClosed(observed)) => Err(
                StrictPrepareError::NotClosed(strict_status_of(observed, request.0)),
            ),
        }
    }

    /// RR R1 跨 crate 薄入口：受检请求状态查询。先校验 strict 请求域（非法输入直接
    /// `Unknown`），再走同源 `channel_state::classify_prepared` 的 Acquire 读取。ORPHANED、
    /// 其它代次、越界值都只是 `Unknown`，不会提升为 `Closed`；也不使用通用
    /// `is_channel_closed` 的 bool。`Closed` 只对持有本次成功 prepare 记录、原 route / pin
    /// 且 channel 未复用的调用方成立。
    fn strict_channel_status(
        &self,
        channel_id: ChannelId,
        request: ChannelState,
    ) -> StrictChannelStatus {
        if !channel_state::is_strict_request(request.0) {
            return StrictChannelStatus::Unknown;
        }
        match channel_state::classify_prepared(
            self.__internal_get_channel_state(channel_id),
            request.0,
        ) {
            channel_state::Prepared::Closed => StrictChannelStatus::Closed,
            channel_state::Prepared::Open => StrictChannelStatus::Open,
            channel_state::Prepared::Unknown(_) => StrictChannelStatus::Unknown,
        }
    }
}

/// The backpressurey handler invoked by a send function when a [`PointerOffset`]
/// cannot be delivered to a [`ZeroCopyReceiver`]
///
/// # Arguments
///
/// * u64: is the retry counter for a delivery incident with a specific sender-receiver pair
/// * Duration: is the elapsed time since the incident was detected
///
/// # Returns
///
/// The [`BackpressureToReceiverAction`] to be taken to mitigate the incident
pub trait BackpressureToReceiverFn: Fn(u64, Duration) -> BackpressureToReceiverAction {}

impl<F: Fn(u64, Duration) -> BackpressureToReceiverAction> BackpressureToReceiverFn for F {}

pub trait ZeroCopySender: Debug + ZeroCopyPortDetails + NamedConcept + Send + Abandonable {
    fn try_send(
        &self,
        ptr: PointerOffset,
        sample_size: usize,
        channel_id: ChannelId,
    ) -> Result<Option<PointerOffset>, ZeroCopySendError>;

    /// RR R1：与 [`ZeroCopySender::try_send()`] 共用唯一真实 push 内核，返回真实回执：
    /// publication（push 到 submission queue）之前的拒绝为 [`TrySendReceipt::NotEnqueued`]，
    /// 之后始终 [`TrySendReceipt::Enqueued`]（可带 `issue`，此时新 offset 已发布、新 used
    /// 已登记，旧 offset 不算回收）。
    fn try_send_observed(
        &self,
        ptr: PointerOffset,
        sample_size: usize,
        channel_id: ChannelId,
    ) -> TrySendReceipt;

    fn blocking_send<F: BackpressureToReceiverFn>(
        &self,
        ptr: PointerOffset,
        sample_size: usize,
        channel_id: ChannelId,
        backpressure_to_receiver_handler: F,
        backpressure_action_for_strategy: BackpressureToReceiverAction,
    ) -> Result<Option<PointerOffset>, ZeroCopySendError>;

    fn reclaim(&self, channel_id: ChannelId)
    -> Result<Option<PointerOffset>, ZeroCopyReclaimError>;

    /// # Safety
    ///
    /// * must ensure that no receiver is still holding data, otherwise data races may occur on
    ///   receiver side
    /// * must ensure that [`ZeroCopySender::try_send()`] and [`ZeroCopySender::blocking_send()`]
    ///   are not called after using this method
    unsafe fn acquire_used_offsets<F: FnMut(PointerOffset)>(&self, callback: F);

    /// RR R2 故障夹具（feature `r2-test-fixtures`，默认关闭）：真实移除一个已发送 offset 的 used
    /// 登记，用于布置 "post-push 旧 used 缺失" 这一真实故障；不伪造回执。目标先受检（channel /
    /// segment 范围、该 (segment, channel) 的 sample_size 已知、offset 对齐、index 在 used 容量内），
    /// 非法目标 typed 拒绝、不触碰任何登记；合法目标返回原是否登记。
    #[cfg(feature = "r2-test-fixtures")]
    fn r2_fixture_forget_used_offset(
        &self,
        ptr: PointerOffset,
        channel_id: ChannelId,
    ) -> Result<bool, FixtureTargetError>;

    /// RR R2 故障夹具：只读查询某 offset 当前是否仍在真实 used 列表中（目标同样先受检）。
    #[cfg(feature = "r2-test-fixtures")]
    fn r2_fixture_is_used(
        &self,
        ptr: PointerOffset,
        channel_id: ChannelId,
    ) -> Result<bool, FixtureTargetError>;

    /// RR R2 故障夹具：安装一次性 overflow 尝试——只对匹配 `(ptr, channel_id)` 的下一次真实发送
    /// 跳过 "非 overflow 且队满" 的 publication 前拒绝，之后仍走同一原 push / 真实挤出 / 真实旧
    /// used 移除与原分类；命中即消费。目标先受检；安装以 CAS 独占（未装 → 安装中 → 已装，Release
    /// 发布目标），命中以 CAS 一次消费（已装 → 未装），因此并发重复安装 / 重复消费各只成功一次；
    /// 已装未消费时拒绝重复安装。仅本进程 Sender 本地状态，不改共享内存布局或共享 overflow 标志。
    #[cfg(feature = "r2-test-fixtures")]
    fn r2_fixture_arm_one_overflow_attempt(
        &self,
        ptr: PointerOffset,
        channel_id: ChannelId,
    ) -> Result<(), FixtureArmError>;

    /// RR R2 故障夹具：只读——当前已安装且未消费的目标。
    #[cfg(feature = "r2-test-fixtures")]
    fn r2_fixture_armed(&self) -> Option<(PointerOffset, ChannelId)>;

    /// RR R2 故障夹具：只读——注入命中（消费）次数。
    #[cfg(feature = "r2-test-fixtures")]
    fn r2_fixture_hits(&self) -> usize;
}

/// RR R2 故障夹具：目标 (offset, channel) 未通过受检（在任何索引 / 除法之前拒绝）。
#[cfg(feature = "r2-test-fixtures")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixtureTargetError {
    /// channel id 超出本连接的 channel 数。
    ChannelOutOfRange,
    /// offset 的 segment id 超出本连接支持的 segment 数。
    SegmentOutOfRange,
    /// 该 (segment, channel) 尚未有任何真实发送，sample_size 未知（为 0），无法定位 used 索引。
    SampleSizeUnknown,
    /// offset 不是 sample_size 的整数倍。
    OffsetNotAligned,
    /// 计算出的 used 索引超出 used 列表容量。
    IndexOutOfRange,
}

/// RR R2 故障夹具：安装一次性 overflow 尝试的失败原因。
#[cfg(feature = "r2-test-fixtures")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixtureArmError {
    /// 目标未通过受检。
    InvalidTarget(FixtureTargetError),
    /// 已有未消费（或正在安装）的目标，不覆盖。
    AlreadyArmed,
}

pub trait ZeroCopyReceiver:
    Debug + ZeroCopyPortDetails + NamedConcept + Send + Abandonable
{
    fn has_data(&self, channel_id: ChannelId) -> bool;
    fn receive(&self, channel_id: ChannelId)
    -> Result<Option<PointerOffset>, ZeroCopyReceiveError>;
    fn release(
        &self,
        ptr: PointerOffset,
        channel_id: ChannelId,
    ) -> Result<(), ZeroCopyReleaseError>;
    fn borrow_count(&self, channel_id: ChannelId) -> usize;
}

pub trait ZeroCopyConnection: Debug + Sized + NamedConceptMgmt {
    type Sender: ZeroCopySender;
    type Receiver: ZeroCopyReceiver;
    type Builder: ZeroCopyConnectionBuilder<Self>;

    /// Removes the [`ZeroCopySender`] forcefully from the [`ZeroCopyConnection`]. This shall only
    /// be called when the [`ZeroCopySender`] died and the connection shall be cleaned up without
    /// causing any problems on the living [`ZeroCopyReceiver`] side.
    ///
    /// # Safety
    ///
    ///  * must ensure that the [`ZeroCopySender`] died while being connected.
    unsafe fn remove_sender(
        name: &FileName,
        config: &Self::Configuration,
    ) -> Result<(), ZeroCopyPortRemoveError>;

    /// Removes the [`ZeroCopyReceiver`] forcefully from the [`ZeroCopyConnection`]. This shall
    /// only be called when the [`ZeroCopySender`] died and the connection shall be cleaned up
    /// without causing any problems on the living [`ZeroCopySender`] side.
    ///
    /// # Safety
    ///
    ///  * must ensure that the [`ZeroCopyReceiver`] died while being connected.
    unsafe fn remove_receiver(
        name: &FileName,
        config: &Self::Configuration,
    ) -> Result<(), ZeroCopyPortRemoveError>;

    /// Returns true if the connection supports safe overflow
    fn does_support_safe_overflow() -> bool {
        false
    }

    /// Returns true if the buffer size of the connection can be configured
    fn has_configurable_buffer_size() -> bool {
        false
    }

    /// The default suffix of every zero copy connection
    fn default_suffix() -> FileName {
        unsafe { FileName::new_unchecked(b".rx") }
    }
}
