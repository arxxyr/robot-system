// Copyright (c) 2025 Contributors to the Eclipse Foundation
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

//! # Example
//!
//! ## Typed API
//!
//! ```
//! use iceoryx2::prelude::*;
//!
//! # fn main() -> Result<(), Box<dyn core::error::Error>> {
//! # let node = NodeBuilder::new().create::<ipc::Service>()?;
//!
//! let service = node
//!    .service_builder(&"My/Funk/ServiceName".try_into()?)
//!    .request_response::<u64, u64>()
//!    .open_or_create()?;
//!
//! let client = service.client_builder()
//!    // defines behavior when server queue is full in a non-overflowing service
//!    .backpressure_strategy(BackpressureStrategy::DiscardData)
//!    .create()?;
//!
//! let request = client.loan_uninit()?;
//! let request = request.write_payload(1829);
//!
//! let pending_response = request.send()?;
//!
//! # Ok(())
//! # }
//! ```
//!
//! ## Slice API
//!
//! ```
//! use iceoryx2::prelude::*;
//!
//! # fn main() -> Result<(), Box<dyn core::error::Error>> {
//! # let node = NodeBuilder::new().create::<ipc::Service>()?;
//!
//! let service = node
//!    .service_builder(&"My/Funk/ServiceName".try_into()?)
//!    .request_response::<[usize], u64>()
//!    .open_or_create()?;
//!
//! let client = service.client_builder()
//!     // provides a hint for the max slice len, 128 means we want at
//!     // list a slice of 128 `usize`
//!     .initial_max_slice_len(128)
//!     // The underlying sample size will be increased with a power of two strategy
//!     // when [`Client::loan_slice()`] or [`Client::loan_slice_uninit()`] requires more
//!     // memory than available.
//!     .allocation_strategy(AllocationStrategy::PowerOfTwo)
//!    .create()?;
//!
//! let number_of_elements = 10;
//! let request = client.loan_slice_uninit(number_of_elements)?;
//! let request = request.write_from_fn(|idx| idx * 2 + 1);
//!
//! let pending_response = request.send()?;
//!
//! # Ok(())
//! # }
//! ```

use crate::active_request::RequestId;
use crate::port::details::chunk::ChunkMut;
use crate::port::details::data_segment_shared_state::DataSegmentSharedState;
use crate::port::port_lifetime_tag::PortLifetimeTag;
use crate::service::header::request_response::RequestHeader;
use crate::service::marker::{CustomHeaderMarker, CustomPayloadMarker, Flatbuffer};
use crate::service::resource::request_response::RequestResponseResources;
use crate::service::static_config::message_type_details::MessageTypeDetails;
use crate::{
    identifiers::UniqueClientId,
    pending_response::PendingResponse,
    port::{
        details::data_segment::DataSegment, port_name::PortName,
        update_connections::UpdateConnections,
    },
    prelude::{BackpressureStrategy, PortFactory},
    request_mut::RequestMut,
    request_mut_uninit::RequestMutUninit,
    service::{
        self,
        dynamic_config::request_response::{ClientDetails, ServerDetails},
        naming_scheme::data_segment_name,
        port_factory::client::{ClientCreateError, LocalClientConfig, PortFactoryClient},
        static_config::message_type_details::TypeVariant,
    },
};
use core::alloc::Layout;
use core::ptr::NonNull;
use core::{any::TypeId, fmt::Debug, marker::PhantomData, mem::MaybeUninit};
use iceoryx2_bb_concurrency::atomic::Ordering;
use iceoryx2_bb_concurrency::atomic::{AtomicBool, AtomicU64, AtomicUsize};
use iceoryx2_bb_concurrency::cell::UnsafeCell;
use iceoryx2_bb_container::slotmap::SlotMapKey;
use iceoryx2_bb_container::{queue::Queue, slotmap::SlotMap};
use iceoryx2_bb_elementary::allocation_strategy::AllocationStrategy;
use iceoryx2_bb_elementary::{CallbackProgression, cyclic_tagger::CyclicTagger};
use iceoryx2_bb_elementary_traits::allocator::{AllocationGrowError, ContentPlacement, Grow};
use iceoryx2_bb_elementary_traits::iceoryx_send::IceoryxSend;
use iceoryx2_bb_elementary_traits::testing::abandonable::Abandonable;
use iceoryx2_bb_elementary_traits::zero_copy_send::ZeroCopySend;
use iceoryx2_cal::bag::{Bag, BagFamily, BagStateFamily};
use iceoryx2_cal::shared_memory::ShmPointer;
use iceoryx2_cal::shm_allocator::PointerOffset;
use iceoryx2_cal::zero_copy_connection::checked_connection_management_size;
use iceoryx2_cal::zero_copy_connection::{CHANNEL_STATE_CLOSED, CHANNEL_STATE_OPEN};
use iceoryx2_cal::{
    arc_sync_policy::ArcSyncPolicy, dynamic_storage::DynamicStorage,
    zero_copy_connection::ChannelId,
};
use iceoryx2_log::{fail, fatal_panic, warn};

use super::{
    LoanError, SendError,
    details::{
        data_segment::{DataSegmentType, StaticSegmentPlanError},
        receiver::{
            Receiver, RetentionPolicy, RoutePin, SenderDetails, try_alloc_empty_table,
            try_alloc_table,
        },
        segment_state::SegmentState,
        sender::{ReceiverDetails, Sender},
    },
    update_connections::ConnectionFailure,
};

/// RR R2：Live 的 Pending 按值持有、Drop 后按值转入固定槽的本地原件（无 Arc、不可 Clone）：
/// 本地 sample 引用、channel 池位置、请求序号与原 pin 原件（响应侧所有权由不可复制的
/// [`RoutePin`] 承担；发送侧只在 send 内按完整原 server id 精确定位，不另存提示）。
#[derive(Debug)]
pub(crate) struct StrictDetached {
    pub(crate) offset: PointerOffset,
    pub(crate) channel_id: ChannelId,
    pub(crate) request_id: RequestId,
    pub(crate) pin: RoutePin,
}

/// RR R2：每个 response channel 一个固定槽（创建时按 override 后真实 N 一次可失败预留）。
#[derive(Debug)]
pub(crate) enum StrictSlot {
    Free,
    /// send 已预留、原 pin 由发送路径本地持有，enqueue 前的过渡态（同一 shared 锁内）。
    Reserved,
    /// 已入队：原 pin 与本地原件由 Live 的 Pending 按值持有。
    Live,
    /// Pending 已 Drop：原件（含原 pin）由本槽接管，等待正常回收或最后 shared 退出处置。
    Detached(StrictDetached),
}

/// Failure that can be emitted when a [`RequestMut`] is sent.
#[derive(Debug, Eq, PartialEq, Copy, Clone)]
pub enum RequestSendError {
    /// Sending this [`RequestMut`] exceeds the maximum supported amount of active
    /// requests. When a [`PendingResponse`] object is released another [`RequestMut`]
    /// can be sent.
    ExceedsMaxActiveRequests,

    /// Underlying [`SendError`]s.
    SendError(SendError),
}

impl From<SendError> for RequestSendError {
    fn from(value: SendError) -> Self {
        RequestSendError::SendError(value)
    }
}

impl From<LoanError> for RequestSendError {
    fn from(value: LoanError) -> Self {
        RequestSendError::SendError(SendError::LoanError(value))
    }
}

impl From<ConnectionFailure> for RequestSendError {
    fn from(value: ConnectionFailure) -> Self {
        RequestSendError::SendError(SendError::ConnectionError(value))
    }
}

impl core::fmt::Display for RequestSendError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "RequestSendError::{self:?}")
    }
}

impl core::error::Error for RequestSendError {}

#[doc(hidden)]
#[derive(Debug)]
pub struct ClientSharedState<Service: service::Service> {
    pub(crate) config: LocalClientConfig,
    pub(crate) request_sender: Sender<Service, RequestResponseResources<Service>>,
    #[cfg(test)]
    _sender_drop_marker: exit_trace::DropMarker,
    pub(crate) response_receiver: Receiver<Service, RequestResponseResources<Service>>,
    #[cfg(test)]
    _receiver_drop_marker: exit_trace::DropMarker,
    client_handle: UnsafeCell<Option<<Service::Bag as BagFamily>::BagHandle>>,
    pub(crate) server_list_state: UnsafeCell<<Service::Bag as BagFamily>::BagState<ServerDetails>>,
    pub(crate) available_channel_ids: UnsafeCell<Queue<ChannelId>>,
    /// RR R2：固定 route / Detached 槽，下标 = channel id，长度 = override 后真实 N。
    pub(crate) strict_slots: UnsafeCell<alloc::vec::Vec<StrictSlot>>,
    /// RR R2：strict 保留策略下连接存储满导致 update 部分失败时置位，下次 update 强制重跑。
    pub(crate) retry_update: AtomicBool,
    pub(crate) active_request_counter: AtomicUsize,
    pub(crate) max_active_requests: usize,
    pub(crate) loan_counter: AtomicUsize,
    max_loans: usize,
    // IMPORTANT!
    // Fields of a rust struct are dropped in declaration order. Since this tag is our marker that the
    // port exists and might require cleanup after a crash, the tag must be defined as last member of
    // the struct.
    // Otherwise the process might crash during cleanup, has already removed the tag but other resources
    // are still existing. This would make a cleanup from another process impossible.
    lifetime_tag: PortLifetimeTag<Service>,
    #[cfg(test)]
    _tag_drop_marker: exit_trace::DropMarker,
}

impl<Service: service::Service> DataSegmentSharedState for ClientSharedState<Service> {
    fn allocation_strategy(&self) -> AllocationStrategy {
        self.request_sender.data_segment.allocation_strategy()
    }

    fn header_len(&self) -> usize {
        self.request_sender.message_type_details.all_headers_len()
    }

    fn message_type_details(&self) -> MessageTypeDetails {
        self.request_sender.message_type_details
    }

    fn payload_size(&self) -> usize {
        self.request_sender.payload_size()
    }

    fn return_loan(&self, offset: PointerOffset) {
        self.request_sender.return_loaned_chunk(offset);
    }
}

impl<Service: service::Service> Grow<ShmPointer> for ClientSharedState<Service> {
    unsafe fn grow(
        &self,
        ptr: ShmPointer,
        old_layout: Layout,
        new_layout: Layout,
        content_placement: ContentPlacement,
    ) -> Result<ShmPointer, AllocationGrowError> {
        match unsafe {
            self.request_sender
                .grow(ptr, old_layout, new_layout, content_placement)
        } {
            Ok(ptr) => Ok(ptr),
            Err(e) => {
                fail!(from self, with e,
                        "Failed to grow request from {old_layout:?} to {new_layout:?}. [{e:?}]");
            }
        }
    }
}

impl<Service: service::Service> Abandonable for ClientSharedState<Service> {
    unsafe fn abandon_in_place(mut this: NonNull<Self>) {
        let this = unsafe { this.as_mut() };
        unsafe { Sender::abandon_in_place(NonNull::from_mut(&mut this.request_sender)) };
        unsafe { Receiver::abandon_in_place(NonNull::from_mut(&mut this.response_receiver)) };
        unsafe {
            Abandonable::abandon_in_place(NonNull::from_mut(&mut this.lifetime_tag));
        }
    }
}

/// RR R2 仅测试：最后 shared 退出的处置顺序记录（线程局部；生产构建不含）。
#[cfg(test)]
pub(crate) mod exit_trace {
    use core::cell::{Cell, RefCell};

    thread_local! {
        static EVENTS: RefCell<alloc::vec::Vec<&'static str>> = const { RefCell::new(alloc::vec::Vec::new()) };
        static DETAILS: RefCell<alloc::vec::Vec<&'static str>> = const { RefCell::new(alloc::vec::Vec::new()) };
        static DETAILS_ENABLED: Cell<bool> = const { Cell::new(false) };
    }

    #[derive(Debug)]
    pub(crate) struct DropMarker(pub(crate) &'static str);

    impl Drop for DropMarker {
        fn drop(&mut self) {
            detail(self.0);
        }
    }

    pub(crate) fn detail(event: &'static str) {
        if DETAILS_ENABLED.with(Cell::get) {
            DETAILS.with(|events| events.borrow_mut().push(event));
        }
    }

    pub(crate) fn start_details() {
        DETAILS.with(|events| events.borrow_mut().clear());
        DETAILS_ENABLED.with(|enabled| enabled.set(true));
    }

    pub(crate) fn take_details() -> alloc::vec::Vec<&'static str> {
        DETAILS_ENABLED.with(|enabled| enabled.set(false));
        DETAILS.with(|events| core::mem::take(&mut *events.borrow_mut()))
    }

    pub(crate) fn record(event: &'static str) {
        EVENTS.with(|events| events.borrow_mut().push(event));
        detail(event);
    }

    pub(crate) fn take() -> alloc::vec::Vec<&'static str> {
        EVENTS.with(|events| core::mem::take(&mut *events.borrow_mut()))
    }
}

impl<Service: service::Service> Drop for ClientSharedState<Service> {
    fn drop(&mut self) {
        // RR R2（r3 §一）：最后 shared 退出——首句按固定槽处置 Detached 的本地原件并消耗原 pin，
        // 之后才取走并注销 handle；随后字段沿声明顺序析构（request_sender → response_receiver →
        // … → lifetime_tag 最后）。此处不 lock、不 clone、不分配、不等待、不 update、不
        // acquire_used_offsets；远端仍持有的引用不冒归还。
        #[cfg(test)]
        exit_trace::record("shared_drop:dispose_detached");
        self.dispose_detached_for_client_exit();
        if let Some(handle) = unsafe { (*self.client_handle.get()).take() } {
            self.request_sender
                .service_state
                .dynamic_storage()
                .get()
                .request_response()
                .release_client_handle(handle);
            #[cfg(test)]
            exit_trace::record("shared_drop:client_handle_released");
        }
        // 此后字段沿声明顺序析构：request_sender → response_receiver → … → port_tag。
        #[cfg(test)]
        exit_trace::record("shared_drop:body_end_fields_follow");
    }
}

impl<Service: service::Service> ClientSharedState<Service> {
    #[allow(clippy::mut_from_ref)]
    pub(crate) fn strict_slots(&self) -> &mut alloc::vec::Vec<StrictSlot> {
        #[deny(clippy::mut_from_ref)]
        unsafe {
            &mut *self.strict_slots.get()
        }
    }

    /// RR R2：唯一按值消费 client 本地原件——本地 sample 引用、active 计数、channel 池位置各一次，
    /// 最后按值消费原 pin 原件。已 Enqueued 的请求在发送时完成了 loan→active 转账，这里不再减
    /// loan；远端登记的引用不在此归还。
    pub(crate) fn release_request_local_parts(&self, detached: StrictDetached) {
        let StrictDetached {
            offset,
            channel_id,
            pin,
            ..
        } = detached;
        self.request_sender.release_chunk(offset);
        #[cfg(test)]
        exit_trace::detail("request:sample_released");
        // 受检的 active 计数下溢 = 不变量损坏（同一原件被消费两次）。
        if self.active_request_counter.load(Ordering::Relaxed) == 0 {
            fatal_panic!(from self,
                "This should never happen! The active request counter would underflow while releasing a strict request.");
        }
        self.active_request_counter.fetch_sub(1, Ordering::Relaxed);
        #[cfg(test)]
        exit_trace::detail("request:active_decremented");
        if !unsafe { &mut *self.available_channel_ids.get() }.push(channel_id) {
            fatal_panic!(from self,
                "This should never happen! The channel id could not be returned to the pool.");
        }
        #[cfg(test)]
        exit_trace::detail("request:channel_returned");
        self.response_receiver.unpin_connection(pin);
        #[cfg(test)]
        exit_trace::detail("request:pin_consumed");
    }

    /// RR R2：Detached 的正常维护（同 shared 锁内，按固定 N 有界）。每个 Detached 先按原 route /
    /// key 有界接收并立即归还无人等待的真实回复，再要求原正常 Closed（Acquire）、channel 真排空、
    /// 真实 CAL borrow_count == 0，才一次释放本地原件并消耗原 pin。Unknown、收包 / 归还错误或
    /// 借用残计数均保持所有权。
    pub(crate) fn maintain_detached(&self) {
        let slots = self.strict_slots();
        let drain_limit = self.response_receiver.buffer_size;
        for slot in slots.iter_mut() {
            let StrictSlot::Detached(detached) = slot else {
                continue;
            };
            let channel_id = detached.channel_id;
            for _ in 0..drain_limit {
                match self.response_receiver.receive_pinned(&detached.pin) {
                    Ok(Some((details, _chunk))) => {
                        self.response_receiver.release_offset(&details, channel_id);
                    }
                    Ok(None) | Err(_) => break,
                }
            }
            let closed = self.response_receiver.strict_status_pinned(&detached.pin)
                == iceoryx2_cal::zero_copy_connection::StrictChannelStatus::Closed;
            if closed
                && !self.response_receiver.has_data_pinned(&detached.pin)
                && self.response_receiver.borrow_count_pinned(&detached.pin) == 0
            {
                let StrictSlot::Detached(detached) = core::mem::replace(slot, StrictSlot::Free)
                else {
                    unreachable!("slot was matched as Detached above");
                };
                self.release_request_local_parts(detached);
            }
        }
        self.response_receiver.maintain_retained();
    }

    /// RR R2（r3 §一）：最后 shared 退出时的本地处置——逐槽 take：先本地原件，再按值消耗原 pin。
    /// 此时不再存在任何 Request / Pending / Response（它们各持 shared 引用），因此只有 Detached
    /// 需要处置；这不是业务退休，也不证明远端已归还。
    fn dispose_detached_for_client_exit(&mut self) {
        let slots = core::mem::take(self.strict_slots.get_mut());
        for slot in slots {
            match slot {
                StrictSlot::Free => (),
                StrictSlot::Reserved | StrictSlot::Live => {
                    // 最后 shared 退出时不可能仍有 Live / 预留中的请求（它们各持 shared 引用）：
                    // 受检失败，不冒成功。
                    fatal_panic!(from self,
                        "This should never happen! A live strict slot survived the last shared reference.");
                }
                StrictSlot::Detached(detached) => {
                    self.release_request_local_parts(detached);
                    #[cfg(test)]
                    exit_trace::record("shared_drop:detached_released");
                }
            }
        }
    }
    pub(crate) fn release_request(&self, was_sample_sent: bool, header: &RequestHeader) {
        if !unsafe { &mut *self.available_channel_ids.get() }.push(header.channel_id) {
            fatal_panic!(from self,
                      "This should never happen! The channel id could not be returned.");
        }
        if !was_sample_sent {
            self.loan_counter.fetch_sub(1, Ordering::Relaxed);
        }
    }

    fn prepare_channel_to_receive_responses(&self, channel_id: ChannelId, request_id: RequestId) {
        self.response_receiver
            .set_channel_state(channel_id, request_id);
    }

    pub(crate) fn send_request(
        &self,
        chunk: &ChunkMut,
        channel_id: ChannelId,
        request_id: RequestId,
    ) -> Result<usize, RequestSendError> {
        let msg = "Unable to send request";

        let active_request_counter = self.active_request_counter.load(Ordering::Relaxed);
        let max_active_requests = match self.config.max_active_requests {
            Some(requests) => requests,
            None => {
                self.request_sender
                    .service_state
                    .static_config()
                    .request_response()
                    .max_active_requests_per_client
            }
        };
        if max_active_requests <= active_request_counter {
            fail!(from self, with RequestSendError::ExceedsMaxActiveRequests,
                    "{} since the number of active requests is limited to {} and sending this request would exceed the limit.", msg, active_request_counter);
        }

        fail!(from self, when self.update_connections(),
            "{} since the connections could not be updated.", msg);

        self.prepare_channel_to_receive_responses(channel_id, request_id);

        let number_of_recipients = match self.request_sender.deliver_offset(
            chunk,
            // All requests are delivered on the same channel, therefore we can use
            // ChannelId::new(0).
            ChannelId::new(0),
        ) {
            Ok(number_of_recipients) => number_of_recipients,
            Err(error) => {
                // No PendingResponse will be created to close this channel, even if
                // the request reached some of the servers before delivery failed.
                self.response_receiver.close_channel(channel_id, request_id);
                return Err(error.into());
            }
        };

        self.active_request_counter.fetch_add(1, Ordering::Relaxed);
        Ok(number_of_recipients)
    }

    pub(crate) fn update_connections(
        &self,
    ) -> Result<(), super::update_connections::ConnectionFailure> {
        let registry_changed = unsafe {
            self.request_sender
                .service_state
                .dynamic_storage()
                .get()
                .request_response()
                .servers
                .update_state(&mut *self.server_list_state.get())
        };
        // RR R2：strict 保留策略下的容量拒绝不消费本次变更，置 dirty 后下次重跑。
        if (registry_changed || self.retry_update.swap(false, Ordering::Relaxed))
            && let Err(e) = self.force_update_connections()
        {
            if e == ConnectionFailure::RetentionCapacityExceeded
                || self.response_receiver.retention == RetentionPolicy::StrictPinned
            {
                self.retry_update.store(true, Ordering::Relaxed);
            }
            fail!(from self, with e,
                    "Connections were updated only partially since at least one connection to a Server port failed.");
        }

        Ok(())
    }

    fn force_update_connections(&self) -> Result<(), ConnectionFailure> {
        let mut result = Ok(());
        self.request_sender.start_update_connection_cycle();
        self.response_receiver.start_update_connection_cycle();

        unsafe {
            (*self.server_list_state.get()).for_each(|index, port| {
                // establish response connection
                let inner_result = self.response_receiver.update_connection(
                    index,
                    SenderDetails {
                        port_id: port.server_id.value(),
                        max_number_of_segments: port.max_number_of_segments,
                        data_segment_type: port.data_segment_type,
                        number_of_chunks: port.number_of_responses,
                    },
                );
                result = result.and(inner_result);

                // establish request connection
                let inner_result = self.request_sender.update_connection(
                    index,
                    ReceiverDetails {
                        port_id: port.server_id.value(),
                        buffer_size: port.request_buffer_size,
                        number_of_channels: 1,
                    },
                    |_| {},
                );
                if let Some(err) = inner_result.err() {
                    result = result.and(Err(err.into()));
                }

                CallbackProgression::Continue
            })
        };

        self.response_receiver.finish_update_connection_cycle();
        self.request_sender.finish_update_connection_cycle();

        result
    }
}

/// Sends [`RequestMut`]s to a [`Server`](crate::port::server::Server) in a
/// request-response based communication.
#[derive(Debug)]
pub struct Client<
    Service: service::Service,
    RequestPayload: Debug + IceoryxSend + ?Sized,
    RequestHeader: Debug + ZeroCopySend,
    ResponsePayload: Debug + IceoryxSend + ?Sized,
    ResponseHeader: Debug + ZeroCopySend,
> {
    pub(crate) client_shared_state: Service::ArcThreadSafetyPolicy<ClientSharedState<Service>>,
    client_details: &'static ClientDetails,
    request_id_counter: AtomicU64,
    _request_payload: PhantomData<RequestPayload>,
    _request_header: PhantomData<RequestHeader>,
    _response_payload: PhantomData<ResponsePayload>,
    _response_header: PhantomData<ResponseHeader>,
}

impl<
    Service: service::Service,
    RequestPayload: Debug + IceoryxSend + ?Sized,
    RequestHeader: Debug + ZeroCopySend,
    ResponsePayload: Debug + IceoryxSend + ?Sized,
    ResponseHeader: Debug + ZeroCopySend,
> Abandonable for Client<Service, RequestPayload, RequestHeader, ResponsePayload, ResponseHeader>
{
    unsafe fn abandon_in_place(mut this: NonNull<Self>) {
        let this = unsafe { this.as_mut() };
        unsafe {
            Service::ArcThreadSafetyPolicy::abandon_in_place(NonNull::from_mut(
                &mut this.client_shared_state,
            ))
        };
    }
}

unsafe impl<
    Service: service::Service,
    RequestPayload: Debug + IceoryxSend + ?Sized,
    RequestHeader: Debug + ZeroCopySend,
    ResponsePayload: Debug + IceoryxSend + ?Sized,
    ResponseHeader: Debug + ZeroCopySend,
> Send for Client<Service, RequestPayload, RequestHeader, ResponsePayload, ResponseHeader>
where
    Service::ArcThreadSafetyPolicy<ClientSharedState<Service>>: Send + Sync,
{
}

unsafe impl<
    Service: service::Service,
    RequestPayload: Debug + IceoryxSend + ?Sized,
    RequestHeader: Debug + ZeroCopySend,
    ResponsePayload: Debug + IceoryxSend + ?Sized,
    ResponseHeader: Debug + ZeroCopySend,
> Sync for Client<Service, RequestPayload, RequestHeader, ResponsePayload, ResponseHeader>
where
    Service::ArcThreadSafetyPolicy<ClientSharedState<Service>>: Send + Sync,
{
}

impl<
    Service: service::Service,
    RequestPayload: Debug + IceoryxSend + ?Sized,
    RequestHeader: Debug + ZeroCopySend,
    ResponsePayload: Debug + IceoryxSend + ?Sized,
    ResponseHeader: Debug + ZeroCopySend,
> Client<Service, RequestPayload, RequestHeader, ResponsePayload, ResponseHeader>
{
    pub(crate) fn new(
        client_factory: PortFactoryClient<
            Service,
            RequestPayload,
            RequestHeader,
            ResponsePayload,
            ResponseHeader,
        >,
        retention: RetentionPolicy,
    ) -> Result<Self, ClientCreateError> {
        let msg = "Unable to create Client port";
        let origin = "Client::new()";
        let service = &client_factory.factory.service;
        let client_id = fail!(from origin,
            when UniqueClientId::new::<Service>(client_factory.config.port_name, service.shared_node().config()),
            with ClientCreateError::UnableToGenerateUniqueClientId, "{msg} since the UniqueClientId could not be generated.");

        // !MUST! be the first thing that is created when a new port is instantiated otherwise the
        // port resources might leak if this process is killed in between.
        let lifetime_tag = PortLifetimeTag::new(
            origin,
            msg,
            client_id.0.value(),
            service.shared_node(),
            ClientCreateError::UnableToCreatePortTag,
        )?;

        let static_config = client_factory.factory.static_config();
        let max_active_requests = match client_factory.config.max_active_requests {
            Some(requests) => {
                if static_config.max_active_requests_per_client < requests {
                    fail!(from origin, with ClientCreateError::MaxActiveRequestsExceedsMaxSupportedActiveRequestsOfService,
                        "{} since the requested max_active_requests {} exceeds the maximum supported active requests {} of the service.",
                    msg, requests, static_config.max_active_requests_per_client);
                }
                requests
            }
            None => static_config.max_active_requests_per_client,
        };
        let rr_config = unsafe { service.static_config().messaging_pattern.request_response() };
        // RR R2：全部派生容量按 override 后真实 N 受检计算；override 有副作用，只调用一次，
        // 其结果统一供数据段 chunk 数、response channel 数、channel 池与固定槽使用。
        let Some(worst_case_number_of_requests) = rr_config
            .required_amount_of_chunks_per_client_data_segment(
                static_config.max_loaned_requests,
                max_active_requests,
            )
        else {
            fail!(from origin, with ClientCreateError::CapacityOverflow,
                "{msg} since the required number of request chunks overflows.");
        };
        let number_of_requests = client_factory
            .preallocate_number_of_requests_override
            .call(worst_case_number_of_requests);
        let server_list = &service.dynamic_storage().get().request_response().servers;

        let global_config = service.shared_node().config();
        let segment_name = data_segment_name(client_id.value());
        let data_segment_type = DataSegmentType::new_from_allocation_strategy(
            client_factory.config.allocation_strategy,
        );
        let max_number_of_segments =
            DataSegment::<Service>::max_number_of_segments(data_segment_type);

        let Some(chunk_layout) = static_config
            .request_message_type_details
            .checked_chunk_layout(client_factory.config.initial_max_slice_len)
        else {
            fail!(from origin, with ClientCreateError::CapacityOverflow,
                "{msg} since the request sample layout for {} elements overflows.",
                client_factory.config.initial_max_slice_len);
        };
        if data_segment_type == DataSegmentType::Static {
            match DataSegment::<Service>::checked_static_segment_plan(
                chunk_layout,
                number_of_requests,
            ) {
                Ok(_) => (),
                Err(StaticSegmentPlanError::Overflow) => {
                    fail!(from origin, with ClientCreateError::CapacityOverflow,
                        "{msg} since the static data segment plan for {} requests overflows.",
                        number_of_requests);
                }
                Err(StaticSegmentPlanError::ExceedsMaxSupportedChunks) => {
                    fail!(from origin, with ClientCreateError::ExceedsMaxSupportedChunks,
                        "{msg} since {} requests exceed the number of pool buckets supported by the allocator.",
                        number_of_requests);
                }
            }
        }
        // RR R2：本端已知的 CAL 连接管理区容量受检——请求连接（本端 sender：samples = 实际 N，
        // buffer = borrow = max_active_requests_per_client，channels = 1）与回复连接的本端下界
        // （channels = 实际 N；peer 的 samples / segments 只有匹配后才知道，由真实 CAL builder 在
        // 创建前受检）。
        if checked_connection_management_size(
            static_config.max_active_requests_per_client,
            static_config.max_active_requests_per_client,
            number_of_requests,
            max_number_of_segments,
            1,
        )
        .is_none()
            || checked_connection_management_size(
                static_config.max_response_buffer_size,
                static_config.max_borrowed_responses_per_pending_response,
                1,
                1,
                number_of_requests,
            )
            .is_none()
        {
            fail!(from origin, with ClientCreateError::CapacityOverflow,
                "{msg} since the zero copy connection management memory for the configured buffer / borrow / channel counts overflows.");
        }

        let number_of_to_be_removed_connections = global_config
            .defaults
            .request_response
            .client_expired_connection_buffer;
        let number_of_active_connections = server_list.capacity();
        let Some(number_of_connections) =
            number_of_to_be_removed_connections.checked_add(number_of_active_connections)
        else {
            fail!(from origin, with ClientCreateError::CapacityOverflow,
                "{msg} since the number of response connections overflows.");
        };

        // 固定容量本地表：共享资源创建前可失败预留，typed 返回。
        let Some(receiver_connections) =
            try_alloc_table(number_of_active_connections, |_| UnsafeCell::new(None))
        else {
            fail!(from origin, with ClientCreateError::InsufficientMemory,
                "{msg} since the response connection table could not be allocated.");
        };
        let Some(to_be_removed_connections) =
            try_alloc_empty_table::<SlotMapKey>(number_of_to_be_removed_connections)
        else {
            fail!(from origin, with ClientCreateError::InsufficientMemory,
                "{msg} since the expired connection buffer could not be allocated.");
        };
        // RR R2：只有 strict 保留策略才申请新表——保留列表沿用原 expired 预算
        // （client_expired_connection_buffer，不扩大）与 override 后真实 N 个固定 route / Detached
        // 槽（一次可失败预留）；Standard 两者置空，保原普通路径。
        let (retained, strict_slots) = match retention {
            RetentionPolicy::Standard => (None, alloc::vec::Vec::new()),
            RetentionPolicy::StrictPinned => {
                let Some(retained) =
                    try_alloc_empty_table::<SlotMapKey>(number_of_to_be_removed_connections)
                else {
                    fail!(from origin, with ClientCreateError::InsufficientMemory,
                        "{msg} since the retained connection list could not be allocated.");
                };
                let mut strict_slots = alloc::vec::Vec::new();
                if strict_slots.try_reserve_exact(number_of_requests).is_err() {
                    fail!(from origin, with ClientCreateError::InsufficientMemory,
                        "{msg} since the {} strict channel slots could not be allocated.",
                        number_of_requests);
                }
                strict_slots.extend((0..number_of_requests).map(|_| StrictSlot::Free));
                (Some(UnsafeCell::new(retained)), strict_slots)
            }
        };

        let data_segment = match data_segment_type {
            DataSegmentType::Static => DataSegment::<Service>::create_static_segment(
                &segment_name,
                chunk_layout,
                global_config,
                number_of_requests,
            ),
            DataSegmentType::Dynamic => DataSegment::<Service>::create_dynamic_segment(
                &segment_name,
                chunk_layout,
                global_config,
                number_of_requests,
                client_factory.config.allocation_strategy,
            ),
        };

        let data_segment = fail!(from origin,
            when data_segment,
            with ClientCreateError::UnableToCreateDataSegment,
            "{} since the client data segment could not be created.", msg);

        let client_details = ClientDetails {
            client_id,
            node_id: *service.shared_node().id(),
            number_of_requests,
            response_buffer_size: static_config.max_response_buffer_size,
            max_slice_len: client_factory.config.initial_max_slice_len,
            data_segment_type,
            max_number_of_segments,
            max_active_requests,
            client_name: client_factory.config.port_name,
        };

        let sender_max_borrowed_chunks = match retention {
            RetentionPolicy::StrictPinned => static_config.max_loaned_requests,
            RetentionPolicy::Standard => static_config
                .required_max_borrowed_chunks_per_client()
                .ok_or(ClientCreateError::CapacityOverflow)?,
        };
        let request_sender = Sender {
            data_segment,
            segment_states: {
                let mut v =
                    alloc::vec::Vec::<SegmentState>::with_capacity(max_number_of_segments as usize);
                for _ in 0..max_number_of_segments {
                    v.push(SegmentState::new(number_of_requests))
                }
                v
            },
            sender_port_id: client_id.value(),
            shared_node: service.shared_node().clone(),
            connections: (0..server_list.capacity())
                .map(|_| UnsafeCell::new(None))
                .collect(),
            receiver_max_buffer_size: static_config.max_active_requests_per_client,
            receiver_max_borrowed_chunks: static_config.max_active_requests_per_client,
            enable_safe_overflow: static_config.enable_safe_overflow_for_requests,
            degradation_handler: client_factory.request_degradation_handler,
            backpressure_handler: client_factory.backpressure_handler,
            number_of_chunks: number_of_requests,
            max_number_of_segments,
            service_state: service.clone(),
            tagger: CyclicTagger::new(),
            loan_counter: AtomicUsize::new(0),
            sender_max_borrowed_chunks,
            backpressure_strategy: client_factory.config.backpressure_strategy,
            message_type_details: static_config.request_message_type_details,
            // all requests are sent via one channel, only the responses require different
            // channels to guarantee that one response does not fill the buffer of another
            // response.
            // but the requests have one shared buffer that the user can configure, therefore
            // one channel suffices
            number_of_channels: 1,
            initial_channel_state: CHANNEL_STATE_OPEN,
        };

        let response_receiver = Receiver {
            connections: receiver_connections,
            receiver_port_id: client_id.value(),
            service_state: service.clone(),
            buffer_size: static_config.max_response_buffer_size,
            tagger: CyclicTagger::new(),
            to_be_removed_connections: UnsafeCell::new(to_be_removed_connections),
            degradation_handler: client_factory.response_degradation_handler,
            message_type_details: static_config.response_message_type_details,
            receiver_max_borrowed_chunks: static_config.max_borrowed_responses_per_pending_response,
            enable_safe_overflow: static_config.enable_safe_overflow_for_responses,
            number_of_channels: number_of_requests,
            connection_storage: UnsafeCell::new(SlotMap::new(number_of_connections)),
            initial_channel_state: CHANNEL_STATE_CLOSED,
            retention,
            retained,
        };

        let client_shared_state = Service::ArcThreadSafetyPolicy::new(ClientSharedState {
            #[cfg(test)]
            _tag_drop_marker: exit_trace::DropMarker("fields:port_tag_dropped"),
            config: client_factory.config,
            client_handle: UnsafeCell::new(None),
            available_channel_ids: {
                let mut queue = Queue::new(number_of_requests);
                for n in 0..number_of_requests {
                    queue.push(ChannelId::new(n));
                }
                UnsafeCell::new(queue)
            },
            request_sender,
            #[cfg(test)]
            _sender_drop_marker: exit_trace::DropMarker("fields:request_sender_dropped"),
            response_receiver,
            #[cfg(test)]
            _receiver_drop_marker: exit_trace::DropMarker("fields:response_receiver_dropped"),
            server_list_state: UnsafeCell::new(unsafe { server_list.get_state() }),
            active_request_counter: AtomicUsize::new(0),
            strict_slots: UnsafeCell::new(strict_slots),
            retry_update: AtomicBool::new(false),
            max_active_requests,
            max_loans: static_config.max_loaned_requests,
            loan_counter: AtomicUsize::new(0),
            lifetime_tag,
        });

        let client_shared_state = match client_shared_state {
            Ok(v) => v,
            Err(e) => {
                fail!(from origin, with ClientCreateError::FailedToDeployThreadsafetyPolicy,
                            "{msg} since the threadsafety policy could not be instantiated ({e:?}).");
            }
        };

        if let Err(e) = client_shared_state.lock().force_update_connections() {
            warn!(from origin,
                "The new Client port is unable to connect to every Server port, caused by {:?}.", e);
        }

        core::sync::atomic::compiler_fence(Ordering::SeqCst);

        // !MUST! be the last task otherwise a client is added to the dynamic config without the
        // creation of all required resources
        let (details, handle) = match service
            .dynamic_storage()
            .get()
            .request_response()
            .register_client_id(client_details)
        {
            Some(v) => v,
            None => {
                fail!(from origin,
                      with ClientCreateError::ExceedsMaxSupportedClients,
                      "{} since it would exceed the maximum support amount of clients of {}.",
                      msg, service.static_config().request_response().max_clients());
            }
        };

        unsafe { *client_shared_state.lock().client_handle.get() = Some(handle) };

        Ok(Self {
            request_id_counter: AtomicU64::new(0),
            client_shared_state,
            client_details: unsafe { &*details },
            _request_payload: PhantomData,
            _request_header: PhantomData,
            _response_payload: PhantomData,
            _response_header: PhantomData,
        })
    }

    /// Returns the [`UniqueClientId`] of the [`Client`]
    pub fn id(&self) -> UniqueClientId {
        self.client_details.client_id
    }

    /// Returns the [`PortName`] of the [`Client`]
    pub fn name(&self) -> &PortName {
        &self.client_details.client_name
    }

    // R0 已验证 nightly 的 try_update 别名从 1.95 提供；仅校准此函数的 Clippy 版本事实。
    #[clippy::msrv = "1.95"]
    fn next_request_id(&self) -> RequestId {
        RequestId::new(
            self.request_id_counter
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                    Some((v + 1) % RequestId::max_value())
                })
                .expect("We return some, therefore the Result always contains a value."),
        )
        .expect("With modulo RequestId::max_value() when incrementing the request id we ensure that the value is always in bounds")
    }

    /// Returns the strategy the [`Client`] follows when a [`RequestMut`] cannot be delivered
    /// if the [`Server`](crate::port::server::Server)s buffer is full.
    pub fn backpressure_strategy(&self) -> BackpressureStrategy {
        self.client_shared_state
            .lock()
            .request_sender
            .backpressure_strategy
    }

    /// Returns the maximal active requests a [`Client`] can send.
    pub fn max_active_requests(&self) -> usize {
        self.client_shared_state.lock().max_active_requests
    }

    fn loan_chunk(&self, slice_len: usize) -> Result<(ChunkMut, ChannelId), LoanError>
    where
        RequestHeader: Default,
    {
        let client_shared_state = self.client_shared_state.lock();
        if client_shared_state.max_loans == client_shared_state.loan_counter.load(Ordering::Relaxed)
        {
            fail!(from self, with LoanError::ExceedsMaxLoans,
                "Unable to loan request since it would exceed the max number of loaned requests ({}).", client_shared_state.max_loans);
        }
        let chunk = client_shared_state
            .request_sender
            .allocate(client_shared_state.request_sender.chunk_layout(slice_len))?;

        let channel_id =
            match unsafe { &mut *client_shared_state.available_channel_ids.get() }.pop() {
                Some(channel_id) => channel_id,
                None => {
                    fatal_panic!(from self,
                    "This should never happen! There are no more available response channels.");
                }
            };

        client_shared_state
            .loan_counter
            .fetch_add(1, Ordering::Relaxed);

        let header_ptr: *mut service::header::request_response::RequestHeader = chunk.header.cast();
        let user_header_ptr: *mut RequestHeader = chunk.user_header.cast();
        unsafe {
            header_ptr.write(service::header::request_response::RequestHeader {
                node_id: *client_shared_state.request_sender.shared_node.id(),
                client_id: self.id(),
                channel_id,
                request_id: self.next_request_id(),
                number_of_elements: slice_len as _,
                payload_offset: 0,
            })
        };
        unsafe { user_header_ptr.write(RequestHeader::default()) };

        Ok((chunk, channel_id))
    }
}

impl<
    Service: service::Service,
    RequestPayload,
    RequestHeader: Default + Debug + ZeroCopySend,
    ResponsePayload: Debug + IceoryxSend + ?Sized,
    ResponseHeader: Debug + ZeroCopySend,
> Client<Service, Flatbuffer<RequestPayload>, RequestHeader, ResponsePayload, ResponseHeader>
{
    /// Acquires a [`RequestMutUninit`] with an integrated flatbuffer builder.
    pub fn loan_flatbuffer(
        &self,
    ) -> Result<
        RequestMutUninit<
            Service,
            Flatbuffer<RequestPayload>,
            RequestHeader,
            ResponsePayload,
            ResponseHeader,
        >,
        LoanError,
    > {
        let (chunk, channel_id) = self.loan_chunk(1)?;

        RequestMutUninit::new_flatbuffer(&self.client_shared_state, chunk, channel_id)
    }
}

impl<
    Service: service::Service,
    RequestPayload: Debug + IceoryxSend + ?Sized,
    RequestHeader: Debug + ZeroCopySend,
    ResponsePayload: Debug + IceoryxSend + ?Sized,
    ResponseHeader: Debug + ZeroCopySend,
> UpdateConnections
    for Client<Service, RequestPayload, RequestHeader, ResponsePayload, ResponseHeader>
{
    fn update_connections(&self) -> Result<(), ConnectionFailure> {
        self.client_shared_state.lock().update_connections()
    }
}

////////////////////////
// BEGIN: typed API
////////////////////////
impl<
    Service: service::Service,
    RequestPayload: Debug + ZeroCopySend,
    RequestHeader: Default + Debug + ZeroCopySend,
    ResponsePayload: Debug + IceoryxSend + ?Sized,
    ResponseHeader: Debug + ZeroCopySend,
> Client<Service, RequestPayload, RequestHeader, ResponsePayload, ResponseHeader>
{
    /// Acquires an [`RequestMutUninit`] to store payload. This API shall be used
    /// by default to avoid unnecessary copies.
    ///
    /// # Example
    ///
    /// ## True Zero Copy
    ///
    /// ```
    /// use iceoryx2::prelude::*;
    ///
    /// # fn main() -> Result<(), Box<dyn core::error::Error>> {
    /// # let node = NodeBuilder::new().create::<ipc::Service>()?;
    /// #
    /// # let service = node
    /// #    .service_builder(&"My/Funk/ServiceName".try_into()?)
    /// #    .request_response::<u64, u64>()
    /// #    .open_or_create()?;
    /// #
    /// # let client = service.client_builder().create()?;
    ///
    /// let mut request = client.loan_uninit()?;
    ///
    /// // Use MaybeUninit API to populate the underlying payload
    /// request.payload_mut().write(1234);
    /// // Promise that we have initialized everything and initialize request
    /// let request = unsafe { request.assume_init() };
    /// // Send request
    /// let pending_response = request.send()?;
    ///
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// ## Copy Payload
    ///
    /// ```
    /// use iceoryx2::prelude::*;
    ///
    /// # fn main() -> Result<(), Box<dyn core::error::Error>> {
    /// # let node = NodeBuilder::new().create::<ipc::Service>()?;
    /// #
    /// # let service = node
    /// #    .service_builder(&"My/Funk/ServiceName".try_into()?)
    /// #    .request_response::<u64, u64>()
    /// #    .open_or_create()?;
    /// #
    /// # let client = service.client_builder().create()?;
    ///
    /// let request = client.loan_uninit()?;
    /// // we write the payload by copying the data into the request and retrieve
    /// // an initialized RequestMut that can be sent
    /// let request = request.write_payload(123);
    /// // Send request
    /// let pending_response = request.send()?;
    ///
    /// # Ok(())
    /// # }
    /// ```
    pub fn loan_uninit(
        &self,
    ) -> Result<
        RequestMutUninit<
            Service,
            MaybeUninit<RequestPayload>,
            RequestHeader,
            ResponsePayload,
            ResponseHeader,
        >,
        LoanError,
    > {
        let (chunk, channel_id) = self.loan_chunk(1)?;

        RequestMutUninit::new(&self.client_shared_state, chunk, channel_id)
    }

    /// Copies the input value into a [`RequestMut`] and sends it. On success it
    /// returns a [`PendingResponse`] that can be used to receive a stream of
    /// [`Response`](crate::response::Response)s from the
    /// [`Server`](crate::port::server::Server).
    pub fn send_copy(
        &self,
        value: RequestPayload,
    ) -> Result<
        PendingResponse<Service, RequestPayload, RequestHeader, ResponsePayload, ResponseHeader>,
        RequestSendError,
    > {
        let msg = "Unable to send copy of request";
        let request = fail!(from self,
                            when self.loan_uninit(),
                            "{} since the loan of the request failed.", msg);

        request.write_payload(value).send()
    }
}

impl<
    Service: service::Service,
    RequestPayload: Debug + Default + ZeroCopySend,
    RequestHeader: Default + Debug + ZeroCopySend,
    ResponsePayload: Debug + IceoryxSend + ?Sized,
    ResponseHeader: Debug + ZeroCopySend,
> Client<Service, RequestPayload, RequestHeader, ResponsePayload, ResponseHeader>
{
    /// Acquires the payload for the request and initializes the underlying memory
    /// with default. This can be very expensive when the payload is large, therefore
    /// prefer [`Client::loan_uninit()`] when possible.
    ///
    /// # Example
    ///
    /// ```
    /// use iceoryx2::prelude::*;
    ///
    /// # fn main() -> Result<(), Box<dyn core::error::Error>> {
    /// # let node = NodeBuilder::new().create::<ipc::Service>()?;
    /// #
    /// # let service = node
    /// #    .service_builder(&"My/Funk/ServiceName".try_into()?)
    /// #    .request_response::<u64, u64>()
    /// #    .open_or_create()?;
    /// #
    /// # let client = service.client_builder().create()?;
    ///
    /// // Acquire request that is initialized with `Default::default()`.
    /// let mut request = client.loan()?;
    /// // Assign a value to the request
    /// *request = 456;
    ///
    /// let pending_response = request.send()?;
    ///
    /// # Ok(())
    /// # }
    /// ```
    pub fn loan(
        &self,
    ) -> Result<
        RequestMut<Service, RequestPayload, RequestHeader, ResponsePayload, ResponseHeader>,
        LoanError,
    > {
        Ok(self.loan_uninit()?.write_payload(RequestPayload::default()))
    }
}

////////////////////////
// END: typed API
////////////////////////

////////////////////////
// BEGIN: sliced API
////////////////////////
impl<
    Service: service::Service,
    RequestPayload: Default + Debug + ZeroCopySend + 'static,
    RequestHeader: Default + Debug + ZeroCopySend,
    ResponsePayload: Debug + IceoryxSend + ?Sized,
    ResponseHeader: Debug + ZeroCopySend,
> Client<Service, [RequestPayload], RequestHeader, ResponsePayload, ResponseHeader>
{
    /// Loans/allocates a [`RequestMut`] from the underlying data segment of the [`Client`]
    /// and initializes all slice elements with the default value. This can be a performance hit
    /// and [`Client::loan_slice_uninit()`] can be used to loan a slice of
    /// [`core::mem::MaybeUninit<Payload>`].
    ///
    /// On failure it returns [`LoanError`] describing the failure.
    ///
    /// # Example
    ///
    /// ```
    /// use iceoryx2::prelude::*;
    ///
    /// # fn main() -> Result<(), Box<dyn core::error::Error>> {
    /// # let node = NodeBuilder::new().create::<ipc::Service>()?;
    /// #
    /// let service = node
    ///    .service_builder(&"My/Funk/ServiceName".try_into()?)
    ///    .request_response::<[u64], u64>()
    ///    .open_or_create()?;
    ///
    /// let client = service.client_builder()
    ///                     .initial_max_slice_len(32)
    ///                     .create()?;
    ///
    /// let slice_length = 13;
    /// let mut request = client.loan_slice(slice_length)?;
    /// for element in request.payload_mut() {
    ///     *element = 1234;
    /// }
    ///
    /// let pending_response = request.send()?;
    ///
    /// # Ok(())
    /// # }
    /// ```
    pub fn loan_slice(
        &self,
        slice_len: usize,
    ) -> Result<
        RequestMut<Service, [RequestPayload], RequestHeader, ResponsePayload, ResponseHeader>,
        LoanError,
    > {
        let request = self.loan_slice_uninit(slice_len)?;
        Ok(request.write_from_fn(|_| RequestPayload::default()))
    }
}

impl<
    Service: service::Service,
    RequestPayload: Debug + ZeroCopySend + 'static,
    RequestHeader: Debug + ZeroCopySend,
    ResponsePayload: Debug + IceoryxSend + ?Sized,
    ResponseHeader: Debug + ZeroCopySend,
> Client<Service, [RequestPayload], RequestHeader, ResponsePayload, ResponseHeader>
{
    /// Returns the maximum initial slice length configured for this [`Client`].
    pub fn initial_max_slice_len(&self) -> usize {
        self.client_shared_state.lock().config.initial_max_slice_len
    }
}

impl<
    Service: service::Service,
    RequestPayload: Debug + ZeroCopySend + 'static,
    RequestHeader: Default + Debug + ZeroCopySend,
    ResponsePayload: Debug + IceoryxSend + ?Sized,
    ResponseHeader: Debug + ZeroCopySend,
> Client<Service, [RequestPayload], RequestHeader, ResponsePayload, ResponseHeader>
{
    /// Loans/allocates a [`RequestMutUninit`] from the underlying data segment of the [`Client`].
    /// The user has to initialize the payload before it can be sent.
    ///
    /// On failure it returns [`LoanError`] describing the failure.
    ///
    /// # Example
    ///
    /// ```
    /// use iceoryx2::prelude::*;
    ///
    /// # fn main() -> Result<(), Box<dyn core::error::Error>> {
    /// # let node = NodeBuilder::new().create::<ipc::Service>()?;
    /// #
    /// let service = node
    ///    .service_builder(&"My/Funk/ServiceName".try_into()?)
    ///    .request_response::<[u64], u64>()
    ///    .open_or_create()?;
    ///
    /// let client = service.client_builder()
    ///                     .initial_max_slice_len(32)
    ///                     .create()?;
    ///
    /// let slice_length = 13;
    /// let mut request = client.loan_slice_uninit(slice_length)?;
    /// for element in request.payload_mut() {
    ///     element.write(1234);
    /// }
    /// // we have written the payload, initialize the request
    /// let request = unsafe { request.assume_init() };
    ///
    /// let pending_response = request.send()?;
    ///
    /// # Ok(())
    /// # }
    /// ```
    #[allow(clippy::type_complexity)] // type alias would require 5 generic parameters which hardly reduces complexity
    pub fn loan_slice_uninit(
        &self,
        slice_len: usize,
    ) -> Result<
        RequestMutUninit<
            Service,
            [MaybeUninit<RequestPayload>],
            RequestHeader,
            ResponsePayload,
            ResponseHeader,
        >,
        LoanError,
    > {
        debug_assert!(TypeId::of::<RequestPayload>() != TypeId::of::<CustomPayloadMarker>());
        unsafe { self.loan_slice_uninit_impl(slice_len) }
    }

    #[allow(clippy::type_complexity)] // type alias would require 5 generic parameters which hardly reduces complexity
    unsafe fn loan_slice_uninit_impl(
        &self,
        slice_len: usize,
    ) -> Result<
        RequestMutUninit<
            Service,
            [MaybeUninit<RequestPayload>],
            RequestHeader,
            ResponsePayload,
            ResponseHeader,
        >,
        LoanError,
    > {
        {
            let client_shared_state = self.client_shared_state.lock();
            let max_slice_len = client_shared_state.config.initial_max_slice_len;
            if client_shared_state.config.allocation_strategy == AllocationStrategy::Static
                && max_slice_len < slice_len
            {
                fail!(from self, with LoanError::ExceedsMaxLoanSize,
                "Unable to loan slice with {} elements since it would exceed the max supported slice length of {}.",
                slice_len, max_slice_len);
            }
        }

        let (chunk, channel_id) = self.loan_chunk(slice_len)?;

        RequestMutUninit::new(&self.client_shared_state, chunk, channel_id)
    }
}

impl<Service: service::Service>
    Client<
        Service,
        [CustomPayloadMarker],
        CustomHeaderMarker,
        [CustomPayloadMarker],
        CustomHeaderMarker,
    >
{
    #[doc(hidden)]
    #[allow(clippy::type_complexity)] // type alias would require 5 generic parameters which hardly reduces complexity
    pub unsafe fn loan_custom_payload(
        &self,
        slice_len: usize,
    ) -> Result<
        RequestMutUninit<
            Service,
            [MaybeUninit<CustomPayloadMarker>],
            CustomHeaderMarker,
            [CustomPayloadMarker],
            CustomHeaderMarker,
        >,
        LoanError,
    > {
        let client_shared_state = self.client_shared_state.lock();

        // TypeVariant::Dynamic == slice and only here it makes sense to loan more than one element
        debug_assert!(
            slice_len == 1
                || client_shared_state.request_sender.payload_type_variant()
                    == TypeVariant::Dynamic
        );

        unsafe { self.loan_slice_uninit_impl(slice_len) }
    }
}
////////////////////////
// END: sliced API
////////////////////////
