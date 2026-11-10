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

use alloc::format;
use core::marker::PhantomData;
use core::ptr::NonNull;

use iceoryx2_bb_concurrency::cell::UnsafeCell;
use iceoryx2_bb_container::slotmap::SlotMap;
use iceoryx2_bb_container::slotmap::SlotMapKey;
use iceoryx2_bb_container::vector::polymorphic_vec::*;
use iceoryx2_bb_elementary::cyclic_tagger::*;
use iceoryx2_bb_elementary_traits::testing::abandonable::Abandonable;
use iceoryx2_bb_memory::heap_allocator::HeapAllocator;
use iceoryx2_cal::named_concept::NamedConceptBuilder;
use iceoryx2_cal::zero_copy_connection::*;
use iceoryx2_log::fatal_panic;
use iceoryx2_log::{error, fail, warn};

use crate::port::DegradationCause;
use crate::port::DegradationInfo;
use crate::port::update_connections::ConnectionFailure;
use crate::port::{DegradationAction, DegradationHandler, ReceiveError};
use crate::service::SharedServiceState;
use crate::service::naming_scheme::data_segment_name;
use crate::service::resource::ServiceResource;
use crate::service::static_config::message_type_details::MessageTypeDetails;
use crate::service::{self, config_scheme::connection_config, naming_scheme::connection_name};

use super::chunk::Chunk;
use super::chunk_details::ChunkDetails;
use super::data_segment::{DataSegmentType, DataSegmentView};

/// 仅故障测试的实际注销账：完整本 receiver 身份隔离各测试，不参与回收判定。
#[cfg(all(test, feature = "r2-test-fixtures"))]
mod unregister_trace {
    use core::cell::RefCell;
    use iceoryx2_cal::shm_allocator::PointerOffset;

    thread_local! {
        static EVENTS: RefCell<alloc::vec::Vec<(u128, PointerOffset)>> = const { RefCell::new(alloc::vec::Vec::new()) };
    }

    pub(super) fn record(receiver: u128, offset: PointerOffset) {
        EVENTS.with(|events| events.borrow_mut().push((receiver, offset)));
    }

    pub(super) fn count(receiver: u128, offset: PointerOffset) -> usize {
        EVENTS.with(|events| {
            events
                .borrow()
                .iter()
                .filter(|item| **item == (receiver, offset))
                .count()
        })
    }
}

#[derive(Clone, Copy)]
pub(crate) struct SenderDetails {
    pub(crate) port_id: u128,
    pub(crate) number_of_chunks: usize,
    pub(crate) max_number_of_segments: u8,
    pub(crate) data_segment_type: DataSegmentType,
}

/// RR R2：连接保留策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetentionPolicy {
    /// 上游原行为：换代 / 消失的连接经 expired 缓冲有界保留，满时可能丢件。
    Standard,
    /// strict：被 pin 的连接保留原 mapping 与绝对 key，不走 expired 缓冲的丢数据 / fatal 分支；
    /// 存储满时 typed 拒绝并由调用方保 dirty 重试。无 pin 且所有 channel 无数据无借用才回收。
    StrictPinned,
}

/// RR R2：原 route 的 pin 原件——只由 [`Receiver::pin_connection`] 在真实 pin 成功时构造；
/// 不可 Clone / Copy、字段私有；绑定本 receiver id、原 sender id、连接绝对 key、channel 与
/// 请求序号。持有期间所指连接不会被回收；只能由 [`Receiver::unpin_connection`] 按值消费一次。
/// 可复制的定位提示（index / server id）可以另存，但不承担所有权。
#[derive(Debug)]
pub(crate) struct RoutePin {
    receiver_port_id: u128,
    sender_port_id: u128,
    key: SlotMapKey,
    channel_id: ChannelId,
    request_id: ChannelState,
}

impl RoutePin {
    /// 仅测试：真实原连接的绝对 SlotMapKey，不改变 pin 所有权。
    #[cfg(test)]
    pub(crate) fn key_for_test(&self) -> usize {
        self.key.value()
    }
}

#[derive(Debug)]
pub(crate) struct Connection<Service: service::Service, Resource: ServiceResource> {
    pub(crate) receiver: <Service::Connection as ZeroCopyConnection>::Receiver,
    pub(crate) data_segment: DataSegmentView<Service>,
    pub(crate) sender_port_id: u128,
    /// RR R2：持有该连接原 route 的 strict 请求数（Live 或 Detached），只由同一 shared 锁内改动。
    pin_count: usize,
    tag: Tag,
    _resource: PhantomData<Resource>,
}

impl<Service: service::Service, Resource: ServiceResource> Abandonable
    for Connection<Service, Resource>
{
    unsafe fn abandon_in_place(mut this: NonNull<Self>) {
        let this = unsafe { this.as_mut() };

        unsafe {
            <Service::Connection as ZeroCopyConnection>::Receiver::abandon_in_place(
                NonNull::from_mut(&mut this.receiver),
            )
        };
        unsafe { DataSegmentView::abandon_in_place(NonNull::from_mut(&mut this.data_segment)) };
    }
}

impl<Service: service::Service, Resource: ServiceResource> Taggable
    for Connection<Service, Resource>
{
    fn tag(&self) -> &Tag {
        &self.tag
    }
}

impl<Service: service::Service, Resource: ServiceResource> Connection<Service, Resource> {
    fn new(
        this: &Receiver<Service, Resource>,
        data_segment_type: DataSegmentType,
        sender_port_id: u128,
        number_of_chunks: usize,
        max_number_of_segments: u8,
        cyclic_tagger: &CyclicTagger,
        initial_channel_state: ChannelState,
    ) -> Result<Self, ConnectionFailure> {
        let msg = format!(
            "Unable to establish connection to sender port {:?} from receiver port {:?}.",
            sender_port_id, this.receiver_port_id
        );

        let global_config = this.service_state.shared_node().config();
        let receiver = fail!(from this,
                        when <Service::Connection as ZeroCopyConnection>::
                            Builder::new( &connection_name(sender_port_id, this.receiver_port_id))
                                    .config(&connection_config::<Service>(global_config))
                                    .buffer_size(this.buffer_size)
                                    .receiver_max_borrowed_chunks_per_channel(this.receiver_max_borrowed_chunks)
                                    .enable_safe_overflow(this.enable_safe_overflow)
                                    .number_of_chunks_per_segment(number_of_chunks)
                                    .number_of_channels(this.number_of_channels)
                                    .initial_channel_state(initial_channel_state)
                                    .max_supported_shared_memory_segments(max_number_of_segments)
                                    .timeout(global_config.global.creation_timeout)
                                    .create_receiver(),
                        "{} since the zero copy connection could not be established.", msg);

        let segment_name = data_segment_name(sender_port_id);
        let data_segment = match data_segment_type {
            DataSegmentType::Static => {
                DataSegmentView::open_static_segment(&segment_name, global_config)
            }
            DataSegmentType::Dynamic => {
                DataSegmentView::open_dynamic_segment(&segment_name, global_config)
            }
        };

        let data_segment = fail!(from this,
                                 when data_segment,
                                "{} since the sender data segment could not be opened.", msg);

        Ok(Self {
            receiver,
            data_segment,
            sender_port_id,
            pin_count: 0,
            tag: cyclic_tagger.create_tag(),
            _resource: PhantomData,
        })
    }

    fn has_data_or_borrows(&self) -> bool {
        let (has_data, has_borrows) =
            Receiver::<Service, Resource>::receiver_channels_have_data_or_borrows(&self.receiver);
        has_data | has_borrows
    }
}

#[derive(Debug)]
pub(crate) struct Receiver<Service: service::Service, Resource: ServiceResource> {
    pub(crate) connections: PolymorphicVec<'static, UnsafeCell<Option<SlotMapKey>>, HeapAllocator>,
    pub(crate) receiver_port_id: u128,
    pub(crate) service_state: SharedServiceState<Service, Resource>,
    pub(crate) buffer_size: usize,
    pub(crate) tagger: CyclicTagger,
    pub(crate) to_be_removed_connections:
        UnsafeCell<PolymorphicVec<'static, SlotMapKey, HeapAllocator>>,
    pub(crate) degradation_handler: DegradationHandler<'static>,
    pub(crate) message_type_details: MessageTypeDetails,
    pub(crate) receiver_max_borrowed_chunks: usize,
    pub(crate) enable_safe_overflow: bool,
    pub(crate) number_of_channels: usize,
    pub(crate) connection_storage: UnsafeCell<SlotMap<Connection<Service, Resource>>>,
    pub(crate) initial_channel_state: ChannelState,
    /// RR R2：保留策略；`StrictPinned` 下换代 / 消失但仍被 pin 或仍有数据 / 借用的连接进入
    /// `retained`（绝对 key），不再由 index 数组引用。
    pub(crate) retention: RetentionPolicy,
    /// `StrictPinned` 下为原 expired 预算 E 的保留表；`Standard` 置 `None`，不承担新表。
    pub(crate) retained: Option<UnsafeCell<PolymorphicVec<'static, SlotMapKey, HeapAllocator>>>,
}

impl<Service: service::Service, Resource: ServiceResource> Abandonable
    for Receiver<Service, Resource>
{
    unsafe fn abandon_in_place(mut this: NonNull<Self>) {
        let this = unsafe { this.as_mut() };

        unsafe { SharedServiceState::abandon_in_place(NonNull::from_mut(&mut this.service_state)) };
    }
}

impl<Service: service::Service, Resource: ServiceResource> Receiver<Service, Resource> {
    /// 仅故障测试：该原 receiver 对指定 offset 实际 unregister 的累计次数。
    #[cfg(all(test, feature = "r2-test-fixtures"))]
    pub(crate) fn unregister_count_for_test(&self, offset: PointerOffset) -> usize {
        unregister_trace::count(self.receiver_port_id, offset)
    }

    pub(crate) fn release_offset(&self, chunk: &ChunkDetails, channel_id: ChannelId) {
        let connection_storage = unsafe { &mut *self.connection_storage.get() };
        if let Some(connection) = connection_storage.get(chunk.connection_key) {
            if connection.sender_port_id != chunk.origin {
                return;
            }

            unsafe { connection.data_segment.unregister_offset(chunk.offset) };
            #[cfg(all(test, feature = "r2-test-fixtures"))]
            unregister_trace::record(self.receiver_port_id, chunk.offset);
            match connection.receiver.release(chunk.offset, channel_id) {
                Ok(()) => (),
                Err(ZeroCopyReleaseError::RetrieveBufferFull) => {
                    error!(from self, "This should never happen! The publishers retrieve channel is full and the chunk cannot be returned.");
                }
            }
        }
    }

    pub(crate) fn set_channel_state(&self, channel_id: ChannelId, state: ChannelState) -> bool {
        let mut ret_val = true;
        let connection_storage = unsafe { &mut *self.connection_storage.get() };
        for (_, connection) in connection_storage.iter() {
            ret_val &= connection.receiver.set_channel_state(channel_id, state)
        }

        ret_val
    }

    pub(crate) fn at_least_one_channel_has_state(
        &self,
        channel_id: ChannelId,
        state: ChannelState,
    ) -> bool {
        let mut ret_val = false;
        let connection_storage = unsafe { &mut *self.connection_storage.get() };
        for (_, connection) in connection_storage.iter() {
            ret_val |= connection.receiver.has_channel_state(channel_id, state);
            if ret_val {
                break;
            }
        }

        ret_val
    }

    pub(crate) fn set_disconnect_hint(&self, channel_id: ChannelId, expected_state: ChannelState) {
        let connection_storage = unsafe { &mut *self.connection_storage.get() };
        for (_, connection) in connection_storage.iter() {
            connection
                .receiver
                .set_disconnect_hint(channel_id, expected_state);
        }
    }

    pub(crate) fn close_channel(&self, channel_id: ChannelId, expected_state: ChannelState) {
        let connection_storage = unsafe { &mut *self.connection_storage.get() };
        for (_, connection) in connection_storage.iter() {
            connection
                .receiver
                .close_channel(channel_id, expected_state);
        }
    }

    pub(crate) fn receiver_port_id(&self) -> u128 {
        self.receiver_port_id
    }

    pub(crate) fn create(
        &self,
        index: usize,
        sender_details: &SenderDetails,
    ) -> Result<(), ConnectionFailure> {
        let connection_storage = unsafe { &mut *self.connection_storage.get() };
        let key = connection_storage.insert(Connection::new(
            self,
            sender_details.data_segment_type,
            sender_details.port_id,
            sender_details.number_of_chunks,
            sender_details.max_number_of_segments,
            &self.tagger,
            self.initial_channel_state,
        )?);
        let key = match key {
            Some(v) => v,
            None => {
                fatal_panic!(from self, "This should never happen! Internal connection storage capacity exceeded.")
            }
        };

        unsafe { *self.connections[index].get() = Some(key) };

        Ok(())
    }

    pub(crate) fn prepare_connection_removal(&self, index: usize) {
        let key = unsafe { *self.connections[index].get() };
        let key = match key {
            None => return,
            Some(key) => key,
        };

        let connection_storage = unsafe { &mut *self.connection_storage.get() };

        if let Some(connection) = connection_storage.get_mut(key) {
            let receiver = &connection.receiver;
            let (connection_has_data, connection_has_borrows) =
                Self::receiver_channels_have_data_or_borrows(receiver);

            let keep_connection = connection_has_data | connection_has_borrows;
            if keep_connection {
                let to_be_removed_connections =
                    unsafe { &mut *self.to_be_removed_connections.get() };
                if to_be_removed_connections.push(key).is_err() {
                    let msg_begin = "Expired connection buffer exceeded.";
                    let msg_disconnect_with_data =
                        "A sender disconnected with undelivered data that will be discarded.";
                    let msg_disconnet_with_borrows = "A sender disconnected with data that is still borrowed. This will lead to segmentation faults when the data is accessed later on.";
                    let msg_end = "Increase the expired connection buffer to mitigate the problem.";

                    // push failed! let's see if there is something that can be removed
                    let index_and_key_to_remove = Self::find_connection_without_data_and_borrows(
                        connection_storage,
                        to_be_removed_connections,
                    );

                    if let Some((index, key)) = index_and_key_to_remove {
                        // we found a connection without data and borrows which can be removed from the container
                        to_be_removed_connections.remove(index);
                        connection_storage.remove(key);
                    } else if connection_has_borrows {
                        // we did not find a connection that can safely be removed to create some space for the connection with borrows;
                        // removing a connection with borrows might lead to segfaults -> try to remove a connection without borrows
                        let index_and_key_to_remove = Self::find_connection_without_borrows(
                            connection_storage,
                            to_be_removed_connections,
                        );

                        if let Some((index, key)) = index_and_key_to_remove {
                            // we found a connection without borrows which can be removed from the container
                            warn!(from self, "{} {} {}", msg_begin, msg_disconnect_with_data, msg_end);
                            to_be_removed_connections.remove(index);
                            connection_storage.remove(key);
                        }
                    }

                    if to_be_removed_connections.push(key).is_err() {
                        if connection_has_borrows {
                            fatal_panic!(from self, "This should never happen! {} {} {}", msg_begin, msg_disconnet_with_borrows, msg_end);
                        } else {
                            warn!(from self, "{} {} {}", msg_begin, msg_disconnect_with_data, msg_end);
                        }
                        connection_storage.remove(key);
                    }
                }
            } else {
                connection_storage.remove(key);
            }
        }
    }

    fn find_connection_without_data_and_borrows(
        connection_storage: &SlotMap<Connection<Service, Resource>>,
        connections: &PolymorphicVec<'static, SlotMapKey, HeapAllocator>,
    ) -> Option<(usize, SlotMapKey)> {
        Self::find_connection_with_condition(
            connection_storage,
            connections,
            |has_data, has_borrows| !(has_data | has_borrows),
        )
    }

    fn find_connection_without_borrows(
        connection_storage: &SlotMap<Connection<Service, Resource>>,
        connections: &PolymorphicVec<'static, SlotMapKey, HeapAllocator>,
    ) -> Option<(usize, SlotMapKey)> {
        Self::find_connection_with_condition(
            connection_storage,
            connections,
            |_has_data, has_borrows| !has_borrows,
        )
    }

    fn find_connection_with_condition<C: Fn(bool, bool) -> bool>(
        connection_storage: &SlotMap<Connection<Service, Resource>>,
        connections: &PolymorphicVec<'static, SlotMapKey, HeapAllocator>,
        condition: C,
    ) -> Option<(usize, SlotMapKey)> {
        let mut index_and_key = None;
        for (n, &connection_key) in connections.iter().enumerate() {
            let connection = match connection_storage.get(connection_key) {
                Some(connection) => connection,
                None => {
                    index_and_key = Some((n, connection_key));
                    break;
                }
            };

            let receiver = &connection.receiver;
            let (has_data, has_borrows) = Self::receiver_channels_have_data_or_borrows(receiver);

            if condition(has_data, has_borrows) {
                index_and_key = Some((n, connection_key));
                break;
            }
        }

        index_and_key
    }

    fn receiver_channels_have_data_or_borrows(
        receiver: &<Service::Connection as ZeroCopyConnection>::Receiver,
    ) -> (bool, bool) {
        let mut has_data = false;
        let mut has_borrows = false;
        for id in 0..receiver.number_of_channels() {
            let channel_id = ChannelId::new(id);
            has_data |= receiver.has_data(channel_id);
            has_borrows |= receiver.borrow_count(channel_id) > 0;
            if has_data && has_borrows {
                break;
            }
        }

        (has_data, has_borrows)
    }

    pub(crate) fn remove_connection(&self, index: usize) {
        self.prepare_connection_removal(index);
        unsafe { *self.connections[index].get() = None };
    }

    pub(crate) fn has_chunks(&self, channel_id: ChannelId) -> bool {
        let connection_storage = unsafe { &mut *self.connection_storage.get() };
        for (_, connection) in connection_storage.iter() {
            if connection.receiver.has_data(channel_id) {
                return true;
            }
        }

        false
    }

    pub(crate) fn has_chunks_in_active_connection(&self, channel_id: ChannelId) -> bool {
        let connection_storage = unsafe { &mut *self.connection_storage.get() };
        for connection_key in self.connections.iter() {
            if let Some(connection_key) = unsafe { &*connection_key.get() }
                && let Some(connection) = connection_storage.get(*connection_key)
                && connection.receiver.has_data(channel_id)
            {
                return true;
            }
        }
        false
    }

    fn receive_from_connection(
        &self,
        connection: &Connection<Service, Resource>,
        connection_key: SlotMapKey,
        channel_id: ChannelId,
    ) -> Result<Option<(ChunkDetails, Chunk)>, ReceiveError> {
        let msg = "Unable to receive another chunk";

        match connection.receiver.receive(channel_id) {
            Ok(data) => match data {
                None => Ok(None),
                Some(offset) => {
                    let details = ChunkDetails {
                        connection_key,
                        offset,
                        origin: connection.sender_port_id,
                    };

                    let offset = match connection
                        .data_segment
                        .register_and_translate_offset(offset)
                    {
                        Ok(offset) => offset,
                        Err(e) => {
                            if connection.data_segment.is_dynamic() {
                                warn!(from self, "Lost chunk. This only happens in the dynamic use case when a sender has reallocated its data segment and gone out of scope before the receiver has mapped the realloacted data segment. To circumvent this, you could either use static memory or increase the initial max slice len.");
                                if let Err(e) = connection.receiver.release(offset, channel_id) {
                                    error!(from self,
                                        "This should never happen! Failed to return the lost chunk that was received before. [{e:?}]");
                                }
                                return Ok(None);
                            }
                            fail!(from self, with ReceiveError::ConnectionFailure(ConnectionFailure::UnableToMapSendersDataSegment(e)),
                                "Unable to register and translate offset from sender {:?} since the received offset {:?} could not be registered and translated.",
                                connection.sender_port_id, offset);
                        }
                    };

                    Ok(Some((
                        details,
                        Chunk::new(&self.message_type_details, offset),
                    )))
                }
            },
            Err(ZeroCopyReceiveError::ReceiveWouldExceedMaxBorrowValue) => {
                fail!(from self, with ReceiveError::ExceedsMaxBorrows,
                    "{} since it would exceed the maximum {} of borrowed chunks.",
                    msg, connection.receiver.max_borrowed_chunks());
            }
        }
    }

    fn receive_from_to_be_removed_connections(
        &self,
        channel_id: ChannelId,
    ) -> Result<Option<(ChunkDetails, Chunk)>, ReceiveError> {
        let mut ret_val = None;
        let to_be_removed_connections = unsafe { &mut *self.to_be_removed_connections.get() };

        if !to_be_removed_connections.is_empty() {
            let connection_storage = unsafe { &mut *self.connection_storage.get() };

            let mut indices_to_skip = 0;
            // loop through all 'to_be_removed_connections' break the for-loop if a connection needs to be removed;
            // then continue the for-loop but skip the previously looped indices
            loop {
                let mut index_and_key = None;
                for (n, connection_key) in to_be_removed_connections
                    .iter()
                    .skip(indices_to_skip)
                    .enumerate()
                {
                    let connection = match connection_storage.get(*connection_key) {
                        Some(connection) => connection,
                        None => {
                            index_and_key = Some((n, *connection_key));
                            break;
                        }
                    };
                    let receiver = &connection.receiver;

                    if receiver.borrow_count(channel_id) == receiver.max_borrowed_chunks() {
                        continue;
                    }

                    if let Some((details, absolute_address)) =
                        self.receive_from_connection(connection, *connection_key, channel_id)?
                    {
                        ret_val = Some((details, absolute_address));
                        break;
                    } else {
                        let (_has_data, has_borrows) =
                            Self::receiver_channels_have_data_or_borrows(receiver);

                        if !has_borrows {
                            index_and_key = Some((n, *connection_key));
                            break;
                        }
                    }
                }
                // there is a connection which has neither data nor borrows nor is it present in the 'connection_storage'
                if let Some((index, key)) = index_and_key {
                    to_be_removed_connections.remove(index);
                    connection_storage.remove(key);
                    indices_to_skip = index;

                    continue;
                }

                // at this point we either were able to receive a chunk or iterated through all connections
                break;
            }
        }

        Ok(ret_val)
    }

    pub(crate) fn receive(
        &self,
        channel_id: ChannelId,
    ) -> Result<Option<(ChunkDetails, Chunk)>, ReceiveError> {
        if let Some(data) = self.receive_from_to_be_removed_connections(channel_id)? {
            return Ok(Some(data));
        }

        let msg = "Unable to receive data";
        let mut active_channel_count = 0;
        let mut all_channels_exceed_max_borrows = true;
        let connection_storage = unsafe { &*self.connection_storage.get() };
        for (connection_key, connection) in connection_storage.iter() {
            if !connection.receiver.has_data(channel_id) {
                continue;
            }

            active_channel_count += 1;
            if connection.receiver.borrow_count(channel_id)
                >= connection.receiver.max_borrowed_chunks()
            {
                continue;
            } else {
                all_channels_exceed_max_borrows = false;
            }

            if let Some((details, absolute_address)) =
                self.receive_from_connection(connection, connection_key, channel_id)?
            {
                return Ok(Some((details, absolute_address)));
            }
        }

        if all_channels_exceed_max_borrows && active_channel_count != 0 {
            fail!(from self, with ReceiveError::ExceedsMaxBorrows,
                 "{msg} since every channel exceeds the max number of borrows.");
        }

        Ok(None)
    }

    pub(crate) fn start_update_connection_cycle(&self) {
        self.tagger.next_cycle();
    }

    pub(crate) fn update_connection(
        &self,
        index: usize,
        sender_details: SenderDetails,
    ) -> Result<(), ConnectionFailure> {
        let connection_storage = unsafe { &*self.connection_storage.get() };

        let remove_connection = match unsafe { &*self.connections[index].get() } {
            None => true,
            Some(connection_key) => match connection_storage.get(*connection_key) {
                Some(connection) => {
                    let is_connected = connection.sender_port_id == sender_details.port_id;
                    if is_connected {
                        self.tagger.tag(connection);
                    }
                    !is_connected
                }
                None => true,
            },
        };

        if remove_connection {
            match self.retention {
                RetentionPolicy::Standard => self.prepare_connection_removal(index),
                RetentionPolicy::StrictPinned => {
                    // 先完整预检：旧连接若须保留（有 pin / 数据 / 借用），存储与 retained 列表都
                    // 必须有位；不满足时原 key / index / tag 全部不变，typed 拒绝，调用方保 dirty。
                    if !self.strict_has_room_for_replacement(index) {
                        fail!(from self, with ConnectionFailure::RetentionCapacityExceeded,
                            "Unable to establish connection to new sender {:?} since the connection storage is full of retained (pinned / borrowed) connections; keeping existing mappings and retrying later.",
                            sender_details.port_id);
                    }
                    // 预检通过后才提交迁移（不可失败）。
                    self.retire_index_strict(index);
                }
            }

            // strict：连接创建失败必须保真实 Err（调用方保 dirty 重试），不经 degradation 默认 Warn 吞成 Ok。
            if self.retention == RetentionPolicy::StrictPinned {
                return match self.create(index, &sender_details) {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        fail!(from self, with e, "Unable to establish strict connection to new sender {:?}.",
                            sender_details.port_id);
                    }
                };
            }

            match self.create(index, &sender_details) {
                Ok(()) => Ok(()),
                Err(e) => match self.degradation_handler.call(
                    DegradationCause::FailedToEstablishConnection,
                    &DegradationInfo {
                        service_id: self
                            .service_state
                            .static_config()
                            .unique_service_id()
                            .value(),
                        sender_port_id: sender_details.port_id,
                        receiver_port_id: self.receiver_port_id(),
                    },
                ) {
                    DegradationAction::Ignore => Ok(()),
                    DegradationAction::Warn => {
                        warn!(from self, "Unable to establish connection to new sender {:?}.",
                                        sender_details.port_id);
                        Ok(())
                    }
                    DegradationAction::DegradeAndFail => {
                        fail!(from self, with e, "Unable to establish connection to new sender {:?}.",
                                        sender_details.port_id);
                    }
                },
            }
        } else {
            Ok(())
        }
    }

    pub(crate) fn finish_update_connection_cycle(&self) {
        let connection_storage = unsafe { &mut *self.connection_storage.get() };
        for (n, connection_key) in self.connections.iter().enumerate() {
            if let Some(connection_key) = unsafe { &*connection_key.get() }
                && let Some(connection) = connection_storage.get(*connection_key)
                && !connection.was_tagged_by(&self.tagger)
            {
                match self.retention {
                    RetentionPolicy::Standard => self.remove_connection(n),
                    RetentionPolicy::StrictPinned => self.retire_index_strict(n),
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // RR R2：strict 保留策略与按原 key 的访问原语（全部在调用方的 shared 锁内使用）
    // ------------------------------------------------------------------

    /// index 上的旧连接是否须保留（仍被 pin、仍有数据或借用）。
    fn strict_index_must_retain(&self, index: usize) -> Option<(SlotMapKey, bool)> {
        let key = unsafe { *self.connections[index].get() }?;
        let connection_storage = unsafe { &*self.connection_storage.get() };
        let keep = match connection_storage.get(key) {
            Some(connection) => connection.pin_count > 0 || connection.has_data_or_borrows(),
            None => false,
        };
        Some((key, keep))
    }

    /// 预检：替换 index 上的旧连接后，存储与 retained 列表是否都能容纳（不改任何状态）。
    fn strict_has_room_for_replacement(&self, index: usize) -> bool {
        let connection_storage = unsafe { &*self.connection_storage.get() };
        match self.strict_index_must_retain(index) {
            // 旧连接保留：新连接需要一个额外存储位，retained 列表也需要一位。
            Some((_, true)) => {
                let retained_has_room = match &self.retained {
                    Some(retained) => {
                        let retained = unsafe { &*retained.get() };
                        retained.len() < retained.capacity()
                    }
                    None => false,
                };
                !connection_storage.is_full() && retained_has_room
            }
            // 旧连接回收后腾出一位，或本 index 无旧连接：只要存储未满即可。
            Some((_, false)) => true,
            None => !connection_storage.is_full(),
        }
    }

    /// 提交迁移（须先经 `strict_has_room_for_replacement` 预检）：把 index 上的旧连接脱离 index
    /// 数组，须保留者进入 `retained`（保原 mapping 与绝对 key），否则按绝对 key 立即回收。
    fn retire_index_strict(&self, index: usize) {
        let Some((key, keep)) = self.strict_index_must_retain(index) else {
            return;
        };
        if keep {
            let Some(retained) = &self.retained else {
                // 预检已排除（Standard 无保留表）；不破坏旧关联。
                return;
            };
            let retained = unsafe { &mut *retained.get() };
            if retained.len() >= retained.capacity() {
                // 预检已排除；不破坏旧关联，留待下一轮。
                return;
            }
            unsafe { *self.connections[index].get() = None };
            if retained.push(key).is_err() {
                fatal_panic!(from self, "This should never happen! The retained connection list capacity was checked before the push.");
            }
        } else {
            unsafe { *self.connections[index].get() = None };
            unsafe { &mut *self.connection_storage.get() }.remove(key);
        }
    }

    /// 回收已脱离 index、且既无 pin 也无数据无借用的保留连接（绝对 key 删除）。
    pub(crate) fn maintain_retained(&self) {
        let Some(retained) = &self.retained else {
            return;
        };
        let retained = unsafe { &mut *retained.get() };
        let connection_storage = unsafe { &mut *self.connection_storage.get() };
        let mut n = 0;
        while n < retained.len() {
            let key = retained[n];
            let reclaim = match connection_storage.get(key) {
                Some(connection) => connection.pin_count == 0 && !connection.has_data_or_borrows(),
                None => true,
            };
            if reclaim {
                connection_storage.remove(key);
                retained.remove(n);
            } else {
                n += 1;
            }
        }
    }

    /// 仅测试：保留列表长度。
    #[cfg(test)]
    pub(crate) fn retained_len(&self) -> usize {
        self.retained
            .as_ref()
            .map_or(0, |retained| unsafe { &*retained.get() }.len())
    }

    /// 仅测试：连接存储中的连接总数（index 数组 + 保留者）。
    #[cfg(test)]
    pub(crate) fn number_of_stored_connections(&self) -> usize {
        unsafe { &*self.connection_storage.get() }.len()
    }

    /// 仅测试：原 pin 原件所指连接的 pin 计数。
    #[cfg(test)]
    pub(crate) fn pin_count_pinned(&self, pin: &RoutePin) -> usize {
        self.pinned(pin).pin_count
    }

    /// 仅测试：借用原 pin 原件所指连接的原 CAL receiver（只读定位，不改状态）。
    #[cfg(all(test, feature = "r2-test-fixtures"))]
    pub(crate) fn cal_receiver_pinned_for_test(
        &self,
        pin: &RoutePin,
    ) -> &<Service::Connection as ZeroCopyConnection>::Receiver {
        &self.pinned(pin).receiver
    }

    /// 仅测试：index 上当前连接的 pin 计数。
    #[cfg(test)]
    pub(crate) fn pin_count_at_index(&self, index: usize) -> Option<usize> {
        let key = unsafe { *self.connections.get(index)?.get() }?;
        unsafe { &*self.connection_storage.get() }
            .get(key)
            .map(|c| c.pin_count)
    }

    /// pin index 上当前连接的原 route（同 shared 锁内）：连接须存在且其 sender id 与
    /// `sender_port_id` 一致；只有真实 pin 成功才构造 [`RoutePin`] 原件。
    pub(crate) fn pin_connection(
        &self,
        index: usize,
        sender_port_id: u128,
        channel_id: ChannelId,
        request_id: ChannelState,
    ) -> Option<RoutePin> {
        let key = unsafe { *self.connections.get(index)?.get() }?;
        let connection = unsafe { &mut *self.connection_storage.get() }.get_mut(key)?;
        if connection.sender_port_id != sender_port_id {
            return None;
        }
        connection.pin_count += 1;
        Some(RoutePin {
            receiver_port_id: self.receiver_port_id,
            sender_port_id,
            key,
            channel_id,
            request_id,
        })
    }

    /// 按值消费原 pin 原件（每个原件恰一次）；不建立新 mapping、不触发回收循环。原件不属于本
    /// receiver、连接已不存在或计数已为 0 均为不变量损坏，受检失败。
    pub(crate) fn unpin_connection(&self, pin: RoutePin) {
        if pin.receiver_port_id != self.receiver_port_id {
            fatal_panic!(from self, "This should never happen! A strict route pin of another receiver was returned here.");
        }
        match unsafe { &mut *self.connection_storage.get() }.get_mut(pin.key) {
            Some(connection) => {
                if connection.sender_port_id != pin.sender_port_id {
                    fatal_panic!(from self, "This should never happen! A strict route pin refers to a connection of another sender.");
                }
                if connection.pin_count == 0 {
                    fatal_panic!(from self, "This should never happen! A strict route pin was returned twice.");
                }
                connection.pin_count -= 1;
            }
            None => {
                fatal_panic!(from self, "This should never happen! A strict route pin refers to a connection that no longer exists.");
            }
        }
    }

    /// 原 pin 原件所指连接（pin 持有期间不会被回收；缺失或身份不符即不变量损坏）。
    fn pinned(&self, pin: &RoutePin) -> &Connection<Service, Resource> {
        if pin.receiver_port_id != self.receiver_port_id {
            fatal_panic!(from self, "This should never happen! A strict route pin of another receiver was used here.");
        }
        match unsafe { &*self.connection_storage.get() }.get(pin.key) {
            Some(connection) if connection.sender_port_id == pin.sender_port_id => connection,
            Some(_) => {
                fatal_panic!(from self, "This should never happen! A pinned strict route refers to a connection of another sender.");
            }
            None => {
                fatal_panic!(from self, "This should never happen! A pinned strict route refers to a connection that no longer exists.");
            }
        }
    }

    /// CAL strict prepare（仅 CLOSED → 原请求），按原件的 channel / 序号。
    pub(crate) fn strict_prepare_pinned(&self, pin: &RoutePin) -> Result<(), StrictPrepareError> {
        self.pinned(pin)
            .receiver
            .strict_prepare(pin.channel_id, pin.request_id)
    }

    /// 严格三态观测，按原件的 channel / 序号。
    pub(crate) fn strict_status_pinned(&self, pin: &RoutePin) -> StrictChannelStatus {
        self.pinned(pin)
            .receiver
            .strict_channel_status(pin.channel_id, pin.request_id)
    }

    /// 只发原 hint（同源 set_hint：只匹配原请求）。
    pub(crate) fn set_disconnect_hint_pinned(&self, pin: &RoutePin) {
        self.pinned(pin)
            .receiver
            .set_disconnect_hint(pin.channel_id, pin.request_id);
    }

    /// 回滚本次成功 prepare（同源 finish：仅原请求 → CLOSED）。
    pub(crate) fn close_channel_pinned(&self, pin: &RoutePin) {
        self.pinned(pin)
            .receiver
            .close_channel(pin.channel_id, pin.request_id);
    }

    pub(crate) fn has_data_pinned(&self, pin: &RoutePin) -> bool {
        self.pinned(pin).receiver.has_data(pin.channel_id)
    }

    pub(crate) fn borrow_count_pinned(&self, pin: &RoutePin) -> usize {
        self.pinned(pin).receiver.borrow_count(pin.channel_id)
    }

    /// 按原 pin 原件直接从原连接、原 channel 收包（不经 expired 列表、不遍历其它连接）。
    pub(crate) fn receive_pinned(
        &self,
        pin: &RoutePin,
    ) -> Result<Option<(ChunkDetails, Chunk)>, ReceiveError> {
        let connection = self.pinned(pin);
        self.receive_from_connection(connection, pin.key, pin.channel_id)
    }

    pub(crate) fn payload_size(&self) -> usize {
        self.message_type_details.payload.size
    }
}

/// RR R2：固定容量本地表的可失败分配——布局溢出或分配失败均返回 `None`，由构造入口 typed 拒绝。
pub(crate) fn try_alloc_table<T, F: FnMut(usize) -> T>(
    capacity: usize,
    f: F,
) -> Option<PolymorphicVec<'static, T, HeapAllocator>> {
    core::alloc::Layout::array::<core::mem::MaybeUninit<T>>(capacity).ok()?;
    PolymorphicVec::from_fn(HeapAllocator::global(), capacity, f).ok()
}

/// RR R2：固定容量空表的可失败分配（同上）。
pub(crate) fn try_alloc_empty_table<T>(
    capacity: usize,
) -> Option<PolymorphicVec<'static, T, HeapAllocator>> {
    core::alloc::Layout::array::<core::mem::MaybeUninit<T>>(capacity).ok()?;
    PolymorphicVec::new(HeapAllocator::global(), capacity).ok()
}
