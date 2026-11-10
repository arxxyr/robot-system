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

#[doc(hidden)]
pub mod details {
    use alloc::vec;
    use alloc::vec::Vec;
    use core::fmt::Debug;
    use core::marker::PhantomData;
    use core::ptr::NonNull;
    use iceoryx2_bb_concurrency::atomic::Ordering;
    use iceoryx2_bb_concurrency::atomic::{AtomicU8, AtomicU64, AtomicUsize};
    use iceoryx2_bb_concurrency::cell::UnsafeCell;
    use iceoryx2_bb_container::vector::relocatable_vec::*;
    use iceoryx2_bb_elementary_traits::allocator::{Allocate, AllocationError};
    use iceoryx2_bb_elementary_traits::relocatable_container::RelocatableContainer;
    use iceoryx2_bb_lock_free::spsc::{
        index_queue::RelocatableIndexQueue,
        safely_overflowing_index_queue::RelocatableSafelyOverflowingIndexQueue,
    };
    use iceoryx2_bb_memory::bump_allocator::BumpAllocator;
    use iceoryx2_bb_posix::adaptive_wait::AdaptiveWaitBuilder;
    use iceoryx2_bb_posix::clock::Time;
    use iceoryx2_bb_posix::file::AccessMode;
    use iceoryx2_log::{fail, fatal_panic};

    pub use crate::zero_copy_connection::*;

    use crate::zero_copy_connection::channel_state;

    use crate::dynamic_storage::{
        DynamicStorage, DynamicStorageBuilder, DynamicStorageCreateError, DynamicStorageOpenError,
        DynamicStorageOpenOrCreateError,
    };
    use crate::named_concept::*;
    use crate::shared_memory::SegmentId;

    use self::used_chunk_list::RelocatableUsedChunkList;

    #[derive(Debug, PartialEq, Eq, Copy)]
    pub struct Configuration<Storage: DynamicStorage<SharedManagementData>> {
        dynamic_storage_config: Storage::Configuration,
        _data: PhantomData<Storage>,
    }

    impl<Storage: DynamicStorage<SharedManagementData>> Clone for Configuration<Storage> {
        fn clone(&self) -> Self {
            Self {
                dynamic_storage_config: self.dynamic_storage_config.clone(),
                _data: PhantomData,
            }
        }
    }

    impl<Storage: DynamicStorage<SharedManagementData>> Default for Configuration<Storage> {
        fn default() -> Self {
            Self {
                dynamic_storage_config: Storage::Configuration::default()
                    .path_hint(&Connection::<Storage>::default_path_hint())
                    .prefix(&Connection::<Storage>::default_prefix())
                    .suffix(&Connection::<Storage>::default_suffix()),
                _data: PhantomData,
            }
        }
    }

    impl<Storage: DynamicStorage<SharedManagementData>> NamedConceptConfiguration
        for Configuration<Storage>
    {
        fn prefix(mut self, value: &FileName) -> Self {
            self.dynamic_storage_config = self.dynamic_storage_config.prefix(value);
            self
        }

        fn get_prefix(&self) -> &FileName {
            self.dynamic_storage_config.get_prefix()
        }

        fn suffix(mut self, value: &FileName) -> Self {
            self.dynamic_storage_config = self.dynamic_storage_config.suffix(value);
            self
        }

        fn path_hint(mut self, value: &Path) -> Self {
            self.dynamic_storage_config = self.dynamic_storage_config.path_hint(value);
            self
        }

        fn get_suffix(&self) -> &FileName {
            self.dynamic_storage_config.get_suffix()
        }

        fn get_path_hint(&self) -> &Path {
            self.dynamic_storage_config.get_path_hint()
        }

        fn path_for(&self, value: &FileName) -> FilePath {
            self.dynamic_storage_config.path_for(value)
        }

        fn extract_name_from_file(&self, value: &FileName) -> Option<FileName> {
            self.dynamic_storage_config.extract_name_from_file(value)
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    #[repr(u8)]
    enum State {
        None = 0b00000000,
        Sender = 0b00000001,
        Receiver = 0b00000010,
        MarkedForDestruction = 0b10000000,
    }

    impl State {
        fn value(&self) -> u8 {
            *self as u8
        }
    }

    fn cleanup_shared_memory<Storage: DynamicStorage<SharedManagementData>>(
        storage: &Storage,
        state_to_remove: State,
    ) {
        let mgmt_data = storage.get();
        if mgmt_data.remove_state(state_to_remove) == State::MarkedForDestruction.value() {
            storage.acquire_ownership()
        }
    }

    #[derive(Debug, ZeroCopySend)]
    #[repr(C)]
    struct SegmentDetails {
        used_chunk_list: RelocatableUsedChunkList,
        sample_size: AtomicUsize,
    }

    impl SegmentDetails {
        fn new_uninit(number_of_samples: usize) -> Self {
            Self {
                used_chunk_list: unsafe { RelocatableUsedChunkList::new_uninit(number_of_samples) },
                sample_size: AtomicUsize::new(0),
            }
        }

        unsafe fn init<T: Allocate<NonNull<u8>>>(
            &mut self,
            allocator: &T,
        ) -> Result<(), AllocationError> {
            unsafe { self.used_chunk_list.init(allocator) }
        }
    }

    /// RR R2：受检镜像 `SharedManagementData::const_memory_size`（与 builder 同样的 clamp、
    /// completion = buffer + borrow + 1、`V(T, n) = size_of::<T>() * n + align_of::<T>() − 1`）。
    /// 任一步溢出、任一内部数组超出 `Layout::array` 表示界或总量超过 `isize::MAX` 均返回 `None`。
    /// 私有 `Channel` / `SegmentDetails` 布局在此单源，调用方不复制。
    pub fn checked_connection_management_size(
        buffer_size: usize,
        max_borrowed_samples_per_channel: usize,
        number_of_samples_per_segment: usize,
        number_of_segments: u8,
        number_of_channels: usize,
    ) -> Option<usize> {
        fn checked_unaligned_mem_size<T>(array_capacity: usize) -> Option<usize> {
            core::alloc::Layout::array::<T>(array_capacity).ok()?;
            core::mem::size_of::<T>()
                .checked_mul(array_capacity)?
                .checked_add(core::mem::align_of::<T>() - 1)
        }

        let buffer_size = buffer_size.max(1);
        let max_borrowed = max_borrowed_samples_per_channel.max(1);
        let samples = number_of_samples_per_segment.max(1);
        let channels = number_of_channels.max(1);
        // 与 builder 的 max_supported_shared_memory_segments 同样 clamp 到至少 1
        let segments = (number_of_segments as usize).max(1);

        let completion_queue_capacity = buffer_size.checked_add(max_borrowed)?.checked_add(1)?;
        // safely overflowing index queue 内部按 capacity + 1 分配
        let submission_queue_slots = buffer_size.checked_add(1)?;
        let channel_size =
            checked_unaligned_mem_size::<UnsafeCell<u64>>(completion_queue_capacity)?.checked_add(
                checked_unaligned_mem_size::<UnsafeCell<u64>>(submission_queue_slots)?,
            )?;
        let segments_times_channels = segments.checked_mul(channels)?;
        let total = channels
            .checked_mul(channel_size)?
            .checked_add(checked_unaligned_mem_size::<Channel>(channels)?)?
            .checked_add(
                checked_unaligned_mem_size::<iceoryx2_bb_concurrency::atomic::AtomicBool>(samples)?
                    .checked_mul(segments_times_channels)?,
            )?
            .checked_add(checked_unaligned_mem_size::<SegmentDetails>(
                segments_times_channels,
            )?)?;
        if total > isize::MAX as usize {
            return None;
        }
        Some(total)
    }

    #[derive(Debug, ZeroCopySend)]
    #[repr(C)]
    struct Channel {
        state: AtomicU64,
        completion_queue: RelocatableIndexQueue,
        submission_queue: RelocatableSafelyOverflowingIndexQueue,
    }

    impl Channel {
        fn new(submission_queue_capacity: usize, completion_queue_capacity: usize) -> Self {
            Self {
                submission_queue: unsafe {
                    RelocatableSafelyOverflowingIndexQueue::new_uninit(submission_queue_capacity)
                },
                completion_queue: unsafe {
                    RelocatableIndexQueue::new_uninit(completion_queue_capacity)
                },
                state: AtomicU64::new(CHANNEL_STATE_OPEN.0),
            }
        }

        fn init(&mut self, allocator: &mut BumpAllocator) {
            let msg = "Failed to initialize channel";
            fatal_panic!(from self, when unsafe { self.submission_queue.init(allocator) },
                        "{} since the submission queue allocation failed. - This is an implementation bug!", msg);
            fatal_panic!(from self, when unsafe { self.completion_queue.init(allocator) },
                        "{} since the completion queue allocation failed. - This is an implementation bug!", msg);
        }
    }

    #[derive(Debug, ZeroCopySend)]
    #[repr(C)]
    pub struct SharedManagementData {
        channels: RelocatableVec<Channel>,
        segment_details: RelocatableVec<SegmentDetails>,
        state: AtomicU8,
        max_borrowed_samples: usize,
        number_of_samples_per_segment: usize,
        number_of_segments: u8,
        enable_safe_overflow: bool,
    }

    impl SharedManagementData {
        fn new(
            enable_safe_overflow: bool,
            max_borrowed_samples: usize,
            number_of_samples_per_segment: usize,
            number_of_segments: u8,
            number_of_channels: usize,
        ) -> Self {
            Self {
                channels: unsafe { RelocatableVec::new_uninit(number_of_channels) },
                segment_details: unsafe {
                    RelocatableVec::new_uninit(number_of_segments as usize * number_of_channels)
                },
                enable_safe_overflow,
                max_borrowed_samples,
                number_of_samples_per_segment,
                number_of_segments,
                state: AtomicU8::new(State::None.value()),
            }
        }

        fn get_segment_details(&self, segment_id: usize, channel_id: usize) -> &SegmentDetails {
            let idx = channel_id * self.number_of_segments as usize + segment_id;
            &self.segment_details[idx]
        }

        fn is_connected(&self) -> bool {
            self.state.load(Ordering::Relaxed) == State::Sender.value() | State::Receiver.value()
        }

        fn remove_state(&self, state_to_remove: State) -> u8 {
            let mut current_state = self.state.load(Ordering::Relaxed);
            if current_state == State::MarkedForDestruction.value() {
                return State::MarkedForDestruction.value();
            }

            loop {
                let new_state = if current_state == state_to_remove.value() {
                    State::MarkedForDestruction.value()
                } else {
                    current_state & !state_to_remove.value()
                };

                match self.state.compare_exchange(
                    current_state,
                    new_state,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        current_state = new_state;
                        break;
                    }
                    Err(s) => {
                        current_state = s;
                    }
                }
            }

            current_state
        }

        fn reserve_port(&self, new_state: u8, msg: &str) -> Result<(), ZeroCopyCreationError> {
            let mut current_state = self.state.load(Ordering::Relaxed);

            loop {
                if current_state & new_state != 0 {
                    fail!(from self, with ZeroCopyCreationError::AnotherInstanceIsAlreadyConnected,
                    "{} since an instance is already connected.", msg);
                } else if current_state & State::MarkedForDestruction.value() != 0 {
                    fail!(from self, with ZeroCopyCreationError::IsBeingCleanedUp,
                    "{} since the connection is currently being cleaned up.", msg);
                }

                match self.state.compare_exchange(
                    current_state,
                    current_state | new_state,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(v) => {
                        current_state = v;
                    }
                }
            }

            Ok(())
        }

        unsafe fn init(
            &mut self,
            allocator: &mut BumpAllocator,
            submission_queue_capacity: usize,
            completion_queue_capacity: usize,
        ) {
            let msg = "Failed to initialize SharedManagementData";
            // initialize channels
            fatal_panic!(from self, when unsafe {self.channels.init(allocator)},
                "{} since the channels vector allocation failed. - This is an implementation bug!", msg);
            for n in 0..self.channels.capacity() {
                unsafe {
                    self.channels.push_unchecked(Channel::new(
                        submission_queue_capacity,
                        completion_queue_capacity,
                    ))
                };
                self.channels[n].init(allocator);
            }

            // initialize segment details
            fatal_panic!(from self, when unsafe { self.segment_details.init(allocator) },
                        "{} since the used chunk list vector allocation failed. - This is an implementation bug!", msg);

            for n in 0..self.segment_details.capacity() {
                if self
                    .segment_details
                    .push(SegmentDetails::new_uninit(
                        self.number_of_samples_per_segment,
                    ))
                    .is_err()
                {
                    fatal_panic!(from self,
                        "{} since the used chunk list could not be added. - This is an implementation bug!", msg);
                }

                fatal_panic!(from self, when unsafe { self.segment_details[n].init(allocator) },
                    "{} since the used chunk list for segment id {} failed to allocate memory. - This is an implementation bug!",
                    msg, n);
            }
        }
    }

    #[derive(Debug)]
    pub struct Builder<Storage: DynamicStorage<SharedManagementData>> {
        name: FileName,
        buffer_size: usize,
        enable_safe_overflow: bool,
        max_borrowed_samples_per_channel: usize,
        number_of_samples_per_segment: usize,
        number_of_segments: u8,
        number_of_channels: usize,
        initial_channel_state: ChannelState,
        timeout: Duration,
        config: Configuration<Storage>,
    }

    impl<Storage: DynamicStorage<SharedManagementData>> Builder<Storage> {
        fn submission_queue_size(&self) -> usize {
            self.buffer_size
        }

        fn completion_queue_size(&self) -> usize {
            self.buffer_size + self.max_borrowed_samples_per_channel + 1
        }

        fn create_or_open_shm(
            &self,
            port_to_register: State,
        ) -> Result<Storage, ZeroCopyCreationError> {
            let msg = "Failed to acquire underlying shared memory";
            // RR R2：管理区字节数按受检算式计算，溢出在创建任何共享资源之前 typed 拒绝。
            let supplementary_size = match checked_connection_management_size(
                self.buffer_size,
                self.max_borrowed_samples_per_channel,
                self.number_of_samples_per_segment,
                self.number_of_segments,
                self.number_of_channels,
            ) {
                Some(size) => size,
                None => {
                    fail!(from self, with ZeroCopyCreationError::CapacityOverflow,
                        "{} since the management memory size for buffer {}, borrowed {}, samples {}, segments {}, channels {} overflows.",
                        msg, self.buffer_size, self.max_borrowed_samples_per_channel,
                        self.number_of_samples_per_segment, self.number_of_segments, self.number_of_channels);
                }
            };
            let storage = <<Storage as DynamicStorage<SharedManagementData>>::Builder<'_> as NamedConceptBuilder<
            Storage,
        >>::new(&self.name)
        .config(&self.config.dynamic_storage_config)
        .timeout(self.timeout)
        .supplementary_size(supplementary_size)
        .initializer(|data, allocator| {
            data.write(
                SharedManagementData::new(
                                        self.enable_safe_overflow,
                                        self.max_borrowed_samples_per_channel,
                                        self.number_of_samples_per_segment,
                                        self.number_of_segments,
                                        self.number_of_channels
                                    )
            );
            let data = unsafe{ data.assume_init_mut() };
            unsafe { data.init(allocator, self.submission_queue_size(), self.completion_queue_size())};
            for channel in data.channels.iter() {
                channel.state.store(self.initial_channel_state.0, Ordering::Relaxed);
            }

            true
        })
        .open_or_create();

            let storage = match storage {
                Ok(storage) => storage,
                Err(DynamicStorageOpenOrCreateError::DynamicStorageCreateError(
                    DynamicStorageCreateError::InsufficientPermissions,
                )) => {
                    fail!(from self, with ZeroCopyCreationError::InsufficientPermissions,
                    "{} due to insufficient permissions to create underlying dynamic storage.", msg);
                }
                Err(DynamicStorageOpenOrCreateError::DynamicStorageOpenError(
                    DynamicStorageOpenError::VersionMismatch,
                )) => {
                    fail!(from self, with ZeroCopyCreationError::VersionMismatch,
                    "{} since the version of the connection does not match.", msg);
                }
                Err(DynamicStorageOpenOrCreateError::DynamicStorageOpenError(
                    DynamicStorageOpenError::InitializationNotYetFinalized,
                )) => {
                    fail!(from self, with ZeroCopyCreationError::InitializationNotYetFinalized,
                    "{} since the initialization of the zero copy connection is not finalized.", msg);
                }
                Err(e) => {
                    fail!(from self, with ZeroCopyCreationError::InternalError,
                    "{} due to an internal failure ({:?}).", msg, e);
                }
            };

            storage.get().reserve_port(port_to_register.value(), msg)?;

            if storage.has_ownership() {
                storage.release_ownership();
            } else {
                let msg = "Failed to open existing connection";

                if storage.get().channels[0].submission_queue.capacity()
                    != self.submission_queue_size()
                {
                    cleanup_shared_memory(&storage, port_to_register);
                    fail!(from self, with ZeroCopyCreationError::IncompatibleBufferSize,
                        "{} since the connection has a buffer size of {} but a buffer size of {} is required.",
                        msg, storage.get().channels[0].submission_queue.capacity(), self.submission_queue_size());
                }

                if storage.get().channels[0].completion_queue.capacity()
                    != self.completion_queue_size()
                {
                    cleanup_shared_memory(&storage, port_to_register);
                    fail!(from self, with ZeroCopyCreationError::IncompatibleMaxBorrowedSamplesPerChannelSetting,
                        "{} since the max borrowed sample per channel setting is set to {} but a value of {} is required.",
                        msg, storage.get().channels[0].completion_queue.capacity() - storage.get().channels[0].submission_queue.capacity(), self.max_borrowed_samples_per_channel);
                }

                if storage.get().enable_safe_overflow != self.enable_safe_overflow {
                    cleanup_shared_memory(&storage, port_to_register);
                    fail!(from self, with ZeroCopyCreationError::IncompatibleOverflowSetting,
                        "{} since the safe overflow is set to {} but should be set to {}.",
                        msg, storage.get().enable_safe_overflow, self.enable_safe_overflow);
                }

                if storage.get().number_of_samples_per_segment != self.number_of_samples_per_segment
                {
                    cleanup_shared_memory(&storage, port_to_register);
                    fail!(from self, with ZeroCopyCreationError::IncompatibleNumberOfSamples,
                        "{} since the requested number of samples is set to {} but should be set to {}.",
                        msg, self.number_of_samples_per_segment, storage.get().number_of_samples_per_segment);
                }

                if storage.get().number_of_segments != self.number_of_segments {
                    cleanup_shared_memory(&storage, port_to_register);
                    fail!(from self, with ZeroCopyCreationError::IncompatibleNumberOfSegments,
                        "{} since the requested number of segments is set to {} but should be set to {}.",
                        msg, self.number_of_segments, storage.get().number_of_segments);
                }

                if storage.get().channels.capacity() != self.number_of_channels {
                    cleanup_shared_memory(&storage, port_to_register);
                    fail!(from self, with ZeroCopyCreationError::IncompatibleNumberOfChannels,
                        "{} since the requested number of channels is set to {} but should be set to {}.",
                        msg, self.number_of_channels, storage.get().channels.capacity());
                }
            }

            Ok(storage)
        }
    }

    impl<Storage: DynamicStorage<SharedManagementData>> NamedConceptBuilder<Connection<Storage>>
        for Builder<Storage>
    {
        fn new(name: &FileName) -> Self {
            Self {
                name: *name,
                buffer_size: DEFAULT_BUFFER_SIZE,
                enable_safe_overflow: DEFAULT_ENABLE_SAFE_OVERFLOW,
                max_borrowed_samples_per_channel: DEFAULT_MAX_BORROWED_SAMPLES_PER_CHANNEL,
                number_of_samples_per_segment: DEFAULT_NUMBER_OF_SAMPLES_PER_SEGMENT,
                number_of_segments: DEFAULT_MAX_SUPPORTED_SHARED_MEMORY_SEGMENTS,
                number_of_channels: DEFAULT_NUMBER_OF_CHANNELS,
                config: Configuration::default(),
                initial_channel_state: CHANNEL_STATE_OPEN,
                timeout: Duration::ZERO,
            }
        }

        fn config(mut self, config: &Configuration<Storage>) -> Self {
            self.config = config.clone();
            self
        }
    }

    impl<Storage: DynamicStorage<SharedManagementData>>
        ZeroCopyConnectionBuilder<Connection<Storage>> for Builder<Storage>
    {
        fn max_supported_shared_memory_segments(mut self, value: u8) -> Self {
            self.number_of_segments = value.clamp(1, u8::MAX);
            self
        }

        fn initial_channel_state(mut self, value: ChannelState) -> Self {
            self.initial_channel_state = value;
            self
        }

        fn buffer_size(mut self, value: usize) -> Self {
            self.buffer_size = value.clamp(1, usize::MAX);
            self
        }

        fn timeout(mut self, value: Duration) -> Self {
            self.timeout = value;
            self
        }

        fn enable_safe_overflow(mut self, value: bool) -> Self {
            self.enable_safe_overflow = value;
            self
        }

        fn number_of_chunks_per_segment(mut self, value: usize) -> Self {
            self.number_of_samples_per_segment = value.clamp(1, usize::MAX);
            self
        }

        fn receiver_max_borrowed_chunks_per_channel(mut self, value: usize) -> Self {
            self.max_borrowed_samples_per_channel = value.clamp(1, usize::MAX);
            self
        }

        fn number_of_channels(mut self, value: usize) -> Self {
            self.number_of_channels = value.clamp(1, usize::MAX);
            self
        }

        fn create_sender(
            self,
        ) -> Result<<Connection<Storage> as ZeroCopyConnection>::Sender, ZeroCopyCreationError>
        {
            let msg = "Unable to create sender";
            let storage = fail!(from self, when self.create_or_open_shm(State::Sender),
            "{} since the corresponding connection could not be created or opened", msg);

            Ok(Sender {
                storage,
                name: self.name,
                #[cfg(feature = "r2-test-fixtures")]
                r2_fixture: R2Fixture::default(),
            })
        }

        fn create_receiver(
            self,
        ) -> Result<<Connection<Storage> as ZeroCopyConnection>::Receiver, ZeroCopyCreationError>
        {
            let msg = "Unable to create receiver";
            let storage = fail!(from self, when self.create_or_open_shm(State::Receiver),
            "{} since the corresponding connection could not be created or opened", msg);

            Ok(Receiver {
                storage,
                borrow_counter: {
                    let mut borrow_counter = vec![];
                    for _ in 0..self.number_of_channels {
                        borrow_counter.push(UnsafeCell::new(0));
                    }
                    borrow_counter
                },
                name: self.name,
            })
        }
    }

    /// RR R2 故障夹具（feature `r2-test-fixtures`）：Sender 进程本地的一次性 overflow 尝试目标；
    /// 全部原子访问，不进入共享内存。所有权用三态 `state`：`UNARMED` → `ARMING`（CAS 独占安装，
    /// 写目标字段）→ `ARMED`（Release 发布）；读取或消费先独占目标，避免重装目标的 ABA。
    #[cfg(feature = "r2-test-fixtures")]
    #[derive(Debug, Default)]
    struct R2Fixture {
        state: AtomicU8,
        offset: AtomicU64,
        channel: AtomicUsize,
        hits: AtomicUsize,
    }

    #[cfg(feature = "r2-test-fixtures")]
    impl R2Fixture {
        const UNARMED: u8 = 0;
        const ARMING: u8 = 1;
        const ARMED: u8 = 2;

        /// CAS 独占安装；已装或安装中即拒绝，不覆盖未消费目标。
        fn arm(&self, ptr: PointerOffset, channel_id: ChannelId) -> Result<(), FixtureArmError> {
            if self
                .state
                .compare_exchange(
                    Self::UNARMED,
                    Self::ARMING,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
            {
                return Err(FixtureArmError::AlreadyArmed);
            }
            self.offset.store(ptr.as_value(), Ordering::Relaxed);
            self.channel.store(channel_id.value(), Ordering::Relaxed);
            self.state.store(Self::ARMED, Ordering::Release);
            Ok(())
        }

        /// 只读快照：先独占目标，避免读取期间被消费和重新安装。
        fn armed(&self) -> Option<(PointerOffset, ChannelId)> {
            if self
                .state
                .compare_exchange(
                    Self::ARMED,
                    Self::ARMING,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
            {
                return None;
            }
            let target = (
                PointerOffset::from_value(self.offset.load(Ordering::Relaxed)),
                ChannelId::new(self.channel.load(Ordering::Relaxed)),
            );
            self.state.store(Self::ARMED, Ordering::Release);
            Some(target)
        }

        /// 先独占再匹配；命中才消费并计 hit，否则完整保留原目标。
        fn consume_if_matches(&self, ptr: PointerOffset, channel_id: ChannelId) -> bool {
            if self
                .state
                .compare_exchange(
                    Self::ARMED,
                    Self::ARMING,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
            {
                return false;
            }
            if self.offset.load(Ordering::Relaxed) != ptr.as_value()
                || self.channel.load(Ordering::Relaxed) != channel_id.value()
            {
                self.state.store(Self::ARMED, Ordering::Release);
                return false;
            }
            self.hits.fetch_add(1, Ordering::Relaxed);
            self.state.store(Self::UNARMED, Ordering::Release);
            true
        }
    }

    #[derive(Debug)]
    pub struct Sender<Storage: DynamicStorage<SharedManagementData>> {
        storage: Storage,
        name: FileName,
        #[cfg(feature = "r2-test-fixtures")]
        r2_fixture: R2Fixture,
    }

    impl<Storage: DynamicStorage<SharedManagementData>> Abandonable for Sender<Storage> {
        unsafe fn abandon_in_place(mut this: NonNull<Self>) {
            let this = unsafe { this.as_mut() };
            unsafe { Storage::abandon_in_place(NonNull::from_mut(&mut this.storage)) };
        }
    }

    impl<Storage: DynamicStorage<SharedManagementData>> Drop for Sender<Storage> {
        fn drop(&mut self) {
            cleanup_shared_memory(&self.storage, State::Sender);
        }
    }

    impl<Storage: DynamicStorage<SharedManagementData>> NamedConcept for Sender<Storage> {
        fn name(&self) -> &FileName {
            &self.name
        }
    }

    impl<Storage: DynamicStorage<SharedManagementData>> ZeroCopyPortDetails for Sender<Storage> {
        fn buffer_size(&self) -> usize {
            self.storage.get().channels[0].submission_queue.capacity()
        }

        fn max_supported_shared_memory_segments(&self) -> u8 {
            self.storage.get().number_of_segments
        }

        fn max_borrowed_chunks(&self) -> usize {
            self.storage.get().max_borrowed_samples
        }

        fn has_enabled_safe_overflow(&self) -> bool {
            self.storage.get().enable_safe_overflow
        }

        fn is_connected(&self) -> bool {
            self.storage.get().is_connected()
        }

        fn number_of_channels(&self) -> usize {
            self.storage.get().channels.capacity()
        }

        fn __internal_get_channel_state(&self, channel_id: ChannelId) -> &AtomicU64 {
            debug_assert!(channel_id.value() < self.storage.get().channels.capacity());
            &self.storage.get().channels[channel_id.value()].state
        }
    }

    impl<Storage: DynamicStorage<SharedManagementData>> Sender<Storage> {
        /// RR R1：唯一真实 push 内核。publication（`submission_queue.push`）之前的拒绝返回
        /// `NotEnqueued`；push 之后无论旧 offset 的 used 移除是否成功都返回 `Enqueued`：
        /// 新 offset 已发布、新 used 已登记，旧 offset 只有真实移除成功才算回收。
        /// `try_send` 与 `try_send_observed` 都只经此处。
        fn push_offset(
            &self,
            ptr: PointerOffset,
            sample_size: usize,
            channel_id: ChannelId,
        ) -> TrySendReceipt {
            debug_assert!(channel_id.value() < self.storage.get().channels.capacity());

            let storage = self.storage.get();

            if !storage.enable_safe_overflow
                && storage.channels[channel_id.value()]
                    .submission_queue
                    .is_full()
            {
                // RR R2 故障夹具：只有匹配的一次性目标才放过 publication 前拒绝，随后仍走同一
                // push / 真实挤出 / 真实旧 used 移除分类；生产构建无此分支。
                #[cfg(feature = "r2-test-fixtures")]
                let injected = self.r2_fixture.consume_if_matches(ptr, channel_id);
                #[cfg(not(feature = "r2-test-fixtures"))]
                let injected = false;
                if !injected {
                    return TrySendReceipt::NotEnqueued(ZeroCopySendError::ReceiveBufferFull);
                }
            }

            let segment_id = ptr.segment_id().value() as usize;
            let segment_details = storage.get_segment_details(segment_id, channel_id.value());
            segment_details
                .sample_size
                .store(sample_size, Ordering::Relaxed);
            debug_assert!(ptr.offset().is_multiple_of(sample_size));
            let index = ptr.offset() / sample_size;

            debug_assert!(segment_id < storage.number_of_segments as usize);

            let did_not_send_same_offset_twice = segment_details.used_chunk_list.insert(index);
            debug_assert!(did_not_send_same_offset_twice);

            match unsafe {
                storage.channels[channel_id.value()]
                    .submission_queue
                    .push(ptr.as_value())
            } {
                Some(v) => {
                    let pointer_offset = PointerOffset::from_value(v);
                    let segment_id = pointer_offset.segment_id().value() as usize;

                    let segment_details =
                        storage.get_segment_details(segment_id, channel_id.value());
                    debug_assert!(
                        pointer_offset
                            .offset()
                            .is_multiple_of(segment_details.sample_size.load(Ordering::Relaxed))
                    );
                    let index = pointer_offset.offset()
                        / segment_details.sample_size.load(Ordering::Relaxed);

                    if segment_details.used_chunk_list.remove(index) {
                        TrySendReceipt::Enqueued {
                            reclaimed_overflow: Some(pointer_offset),
                            issue: None,
                        }
                    } else {
                        TrySendReceipt::Enqueued {
                            reclaimed_overflow: None,
                            issue: Some(EnqueueIssue::OverflowedOffsetNotInUsedList(
                                pointer_offset,
                            )),
                        }
                    }
                }
                None => TrySendReceipt::Enqueued {
                    reclaimed_overflow: None,
                    issue: None,
                },
            }
        }
    }

    #[cfg(test)]
    impl<Storage: DynamicStorage<SharedManagementData>> Sender<Storage> {
        /// 仅测试：从真实 used 列表移除一个已发送 offset 的登记，用于布置“post-push 旧 used
        /// 缺失”这一真实故障；不伪造任何回执。返回原列表是否确有该登记。
        pub(crate) fn test_only_forget_used_offset(
            &self,
            ptr: PointerOffset,
            channel_id: ChannelId,
        ) -> bool {
            let storage = self.storage.get();
            let segment_details =
                storage.get_segment_details(ptr.segment_id().value() as usize, channel_id.value());
            let index = ptr.offset() / segment_details.sample_size.load(Ordering::Relaxed);
            segment_details.used_chunk_list.remove(index)
        }

        /// 仅测试：只读查询某个 offset 当前是否仍在真实 used 列表中；活借用期间的检查只能
        /// 用它，不能用破坏性的 `acquire_used_offsets`。
        pub(crate) fn test_only_is_used(&self, ptr: PointerOffset, channel_id: ChannelId) -> bool {
            let storage = self.storage.get();
            let segment_details =
                storage.get_segment_details(ptr.segment_id().value() as usize, channel_id.value());
            let index = ptr.offset() / segment_details.sample_size.load(Ordering::Relaxed);
            segment_details.used_chunk_list.contains(index)
        }
    }

    #[cfg(feature = "r2-test-fixtures")]
    impl<Storage: DynamicStorage<SharedManagementData>> Sender<Storage> {
        /// 夹具目标受检（在任何索引 / 除法之前）：channel / segment 范围、sample_size 已知、
        /// offset 对齐、used 索引在容量内。合法即返回该 (segment, channel) 的 SegmentDetails 与索引。
        fn r2_fixture_target(
            &self,
            ptr: PointerOffset,
            channel_id: ChannelId,
        ) -> Result<(&SegmentDetails, usize), FixtureTargetError> {
            let storage = self.storage.get();
            if channel_id.value() >= storage.channels.capacity() {
                return Err(FixtureTargetError::ChannelOutOfRange);
            }
            let segment_id = ptr.segment_id().value() as usize;
            if segment_id >= storage.number_of_segments as usize
                || segment_id >= storage.segment_details.len()
            {
                return Err(FixtureTargetError::SegmentOutOfRange);
            }
            let segment_details = storage.get_segment_details(segment_id, channel_id.value());
            let sample_size = segment_details.sample_size.load(Ordering::Relaxed);
            if sample_size == 0 {
                return Err(FixtureTargetError::SampleSizeUnknown);
            }
            if !ptr.offset().is_multiple_of(sample_size) {
                return Err(FixtureTargetError::OffsetNotAligned);
            }
            let index = ptr.offset() / sample_size;
            if index >= segment_details.used_chunk_list.capacity() {
                return Err(FixtureTargetError::IndexOutOfRange);
            }
            Ok((segment_details, index))
        }
    }

    impl<Storage: DynamicStorage<SharedManagementData>> ZeroCopySender for Sender<Storage> {
        fn try_send(
            &self,
            ptr: PointerOffset,
            sample_size: usize,
            channel_id: ChannelId,
        ) -> Result<Option<PointerOffset>, ZeroCopySendError> {
            let msg = "Unable to send sample";

            // 原 Result 投影：两条原错误路径保留原日志，其余由 into_result 统一映射。
            match self.push_offset(ptr, sample_size, channel_id) {
                TrySendReceipt::NotEnqueued(ZeroCopySendError::ReceiveBufferFull) => {
                    fail!(from self, with ZeroCopySendError::ReceiveBufferFull,
                             "{} since the receive buffer is full.", msg);
                }
                TrySendReceipt::Enqueued {
                    issue: Some(EnqueueIssue::OverflowedOffsetNotInUsedList(pointer_offset)),
                    ..
                } => {
                    fail!(from self, with ZeroCopySendError::ConnectionCorrupted,
                        "{} since the invalid offset {:?} was returned on overflow.", msg, pointer_offset);
                }
                receipt => receipt.into_result(),
            }
        }

        fn try_send_observed(
            &self,
            ptr: PointerOffset,
            sample_size: usize,
            channel_id: ChannelId,
        ) -> TrySendReceipt {
            self.push_offset(ptr, sample_size, channel_id)
        }

        fn blocking_send<F: BackpressureToReceiverFn>(
            &self,
            ptr: PointerOffset,
            sample_size: usize,
            channel_id: ChannelId,
            backpressure_to_receiver_handler: F,
            backpressure_action_for_strategy: BackpressureToReceiverAction,
        ) -> Result<Option<PointerOffset>, ZeroCopySendError> {
            let msg = "Unable to blocking send the offset";
            debug_assert!(channel_id.value() < self.storage.get().channels.capacity());

            let mgmt = self.storage.get();
            if !mgmt.enable_safe_overflow
                && mgmt.channels[channel_id.value()].submission_queue.is_full()
            {
                let mut is_connected = false;
                let mut has_valid_channel_state = false;
                let mut do_fail = false;
                let mut retry_until_delivered = false;
                let mut retry_counter = 0;
                let start = Time::now().unwrap();

                const WAIT_CONTINUE: bool = true;
                const WAIT_ABORT: bool = false;

                if let Err(e) = AdaptiveWaitBuilder::new().create().unwrap().wait_while(|| {
                    is_connected = mgmt.is_connected();
                    // RR R1：CLOSED 与私有 ORPHANED 都是终止值（同源 Acquire 读）；满队列不再对
                    // 已被清理者标记的 channel 无限等待。断链优先级与错误分类保持原样。
                    has_valid_channel_state =
                        !channel_state::terminal(&mgmt.channels[channel_id.value()].state);
                    if is_connected
                        && has_valid_channel_state
                        && mgmt.channels[channel_id.value()].submission_queue.is_full()
                    {
                        if retry_until_delivered {
                            WAIT_CONTINUE
                        } else {
                            let wait_action = match backpressure_to_receiver_handler(
                                retry_counter,
                                start.elapsed().unwrap_or(Duration::MAX),
                            ) {
                                BackpressureToReceiverAction::FollowBackpressureyStrategy => {
                                    match backpressure_action_for_strategy {
                                        BackpressureToReceiverAction::Retry => {
                                            retry_until_delivered = true;
                                            WAIT_CONTINUE
                                        }
                                        _ => WAIT_ABORT,
                                    }
                                }
                                BackpressureToReceiverAction::Retry => WAIT_CONTINUE,
                                BackpressureToReceiverAction::DiscardPointerOffset => WAIT_ABORT,
                                BackpressureToReceiverAction::DiscardPointerOffsetAndFail => {
                                    do_fail = true;
                                    WAIT_ABORT
                                }
                            };
                            retry_counter += 1;
                            wait_action
                        }
                    } else {
                        WAIT_ABORT
                    }
                }) {
                    fail!(from self, with ZeroCopySendError::InternalError,
                        "{msg} {ptr:?} via channel {channel_id:?} since the adaptive wait failed. [{e:?}]");
                }

                if !is_connected {
                    fail!(from self, with ZeroCopySendError::NoConnectedReceiverAndBufferIsFull,
                        "{msg} {ptr:?} via channel {channel_id:?} since there is no connected receiver and the buffer is full. To prevent a deadlock on sender side the operation is aborted.");
                }

                if !has_valid_channel_state {
                    fail!(from self, with ZeroCopySendError::ChannelIsClosed,
                        "{msg} {ptr:?} via channel {channel_id:?} since the channel is closed.");
                }

                if do_fail {
                    fail!(from self, with ZeroCopySendError::UnableToDeliver,
                          "{msg} {ptr:?} via channel {channel_id:?} since the receive buffer is full.");
                }
            }
            self.try_send(ptr, sample_size, channel_id)
        }

        fn reclaim(
            &self,
            channel_id: ChannelId,
        ) -> Result<Option<PointerOffset>, ZeroCopyReclaimError> {
            debug_assert!(channel_id.value() < self.storage.get().channels.capacity());

            let msg = "Unable to reclaim sample";

            let storage = self.storage.get();
            match unsafe { storage.channels[channel_id.value()].completion_queue.pop() } {
                None => Ok(None),
                Some(v) => {
                    let pointer_offset = PointerOffset::from_value(v);
                    let segment_id = pointer_offset.segment_id().value() as usize;

                    debug_assert!(segment_id < storage.number_of_segments as usize);

                    if segment_id >= storage.segment_details.len() {
                        fail!(from self, with ZeroCopyReclaimError::ReceiverReturnedCorruptedPointerOffset,
                            "{} since the receiver returned a non-existing segment id {:?}.",
                            msg, pointer_offset);
                    }

                    let segment_details =
                        storage.get_segment_details(segment_id, channel_id.value());
                    debug_assert!(
                        pointer_offset
                            .offset()
                            .is_multiple_of(segment_details.sample_size.load(Ordering::Relaxed))
                    );
                    let index = pointer_offset.offset()
                        / segment_details.sample_size.load(Ordering::Relaxed);

                    if !segment_details.used_chunk_list.remove(index) {
                        fail!(from self, with ZeroCopyReclaimError::ReceiverReturnedCorruptedPointerOffset,
                            "{} since the receiver returned a corrupted offset {:?}.",
                            msg, pointer_offset);
                    }
                    Ok(Some(pointer_offset))
                }
            }
        }

        unsafe fn acquire_used_offsets<F: FnMut(PointerOffset)>(&self, mut callback: F) {
            for (n, segment_details) in self.storage.get().segment_details.iter().enumerate() {
                segment_details.used_chunk_list.remove_all(|index| {
                    callback(PointerOffset::from_offset_and_segment_id(
                        index * segment_details.sample_size.load(Ordering::Relaxed),
                        self.segment_id_from_index(n),
                    ))
                });
            }
        }

        #[cfg(feature = "r2-test-fixtures")]
        fn r2_fixture_forget_used_offset(
            &self,
            ptr: PointerOffset,
            channel_id: ChannelId,
        ) -> Result<bool, FixtureTargetError> {
            let (segment_details, index) = self.r2_fixture_target(ptr, channel_id)?;
            Ok(segment_details.used_chunk_list.remove(index))
        }

        #[cfg(feature = "r2-test-fixtures")]
        fn r2_fixture_is_used(
            &self,
            ptr: PointerOffset,
            channel_id: ChannelId,
        ) -> Result<bool, FixtureTargetError> {
            let (segment_details, index) = self.r2_fixture_target(ptr, channel_id)?;
            Ok(segment_details.used_chunk_list.contains(index))
        }

        #[cfg(feature = "r2-test-fixtures")]
        fn r2_fixture_arm_one_overflow_attempt(
            &self,
            ptr: PointerOffset,
            channel_id: ChannelId,
        ) -> Result<(), FixtureArmError> {
            if let Err(e) = self.r2_fixture_target(ptr, channel_id) {
                return Err(FixtureArmError::InvalidTarget(e));
            }
            self.r2_fixture.arm(ptr, channel_id)
        }

        #[cfg(feature = "r2-test-fixtures")]
        fn r2_fixture_armed(&self) -> Option<(PointerOffset, ChannelId)> {
            self.r2_fixture.armed()
        }

        #[cfg(feature = "r2-test-fixtures")]
        fn r2_fixture_hits(&self) -> usize {
            self.r2_fixture.hits.load(Ordering::Relaxed)
        }
    }

    impl<Storage: DynamicStorage<SharedManagementData>> Sender<Storage> {
        fn segment_id_from_index(&self, index: usize) -> SegmentId {
            let storage = self.storage.get();
            let number_of_segments = storage.number_of_segments as usize;
            // the segment details contain an entry for every channel in every segment.
            // so it is in memory a vector of vector where the first index is the
            // channel id and the second is the segment id. this vector is, to make the
            // relocatable construction easier, mapped to a one dimension vector with the
            // same memory layout.
            //
            // example for 3 segments and 4 channels
            //
            // index       segment id      channel id
            // 0           0               0
            // 1           1               0
            // 2           2               0
            // 3           0               1
            // 4           1               1
            // 5           2               1
            // 6           0               2
            // 7           1               2
            // 8           2               2
            // 9           0               3
            // 10          1               3
            // 11          2               3
            //
            // to map an index to a segment id, we need to subtract from index the
            // rounded down value of index which is rounded down to the greatest multiple
            // of number_of_segments.
            SegmentId::new((index - (index / number_of_segments) * number_of_segments) as u8)
        }
    }

    #[derive(Debug)]
    pub struct Receiver<Storage: DynamicStorage<SharedManagementData>> {
        storage: Storage,
        borrow_counter: Vec<UnsafeCell<usize>>,
        name: FileName,
    }

    impl<Storage: DynamicStorage<SharedManagementData>> Abandonable for Receiver<Storage> {
        unsafe fn abandon_in_place(mut this: NonNull<Self>) {
            let this = unsafe { this.as_mut() };
            unsafe { Storage::abandon_in_place(NonNull::from_mut(&mut this.storage)) };
        }
    }

    impl<Storage: DynamicStorage<SharedManagementData>> Drop for Receiver<Storage> {
        fn drop(&mut self) {
            cleanup_shared_memory(&self.storage, State::Receiver);
        }
    }

    impl<Storage: DynamicStorage<SharedManagementData>> Receiver<Storage> {
        #[allow(clippy::mut_from_ref)]
        // convenience to access internal mutable object
        fn borrow_counter(&self, channel_id: ChannelId) -> &mut usize {
            #[deny(clippy::mut_from_ref)]
            unsafe {
                &mut *self.borrow_counter[channel_id.value()].get()
            }
        }
    }

    impl<Storage: DynamicStorage<SharedManagementData>> NamedConcept for Receiver<Storage> {
        fn name(&self) -> &FileName {
            &self.name
        }
    }

    impl<Storage: DynamicStorage<SharedManagementData>> ZeroCopyPortDetails for Receiver<Storage> {
        fn buffer_size(&self) -> usize {
            self.storage.get().channels[0].submission_queue.capacity()
        }

        fn max_supported_shared_memory_segments(&self) -> u8 {
            self.storage.get().number_of_segments
        }

        fn max_borrowed_chunks(&self) -> usize {
            self.storage.get().max_borrowed_samples
        }

        fn has_enabled_safe_overflow(&self) -> bool {
            self.storage.get().enable_safe_overflow
        }

        fn is_connected(&self) -> bool {
            self.storage.get().is_connected()
        }

        fn number_of_channels(&self) -> usize {
            self.storage.get().channels.capacity()
        }

        fn __internal_get_channel_state(&self, channel_id: ChannelId) -> &AtomicU64 {
            debug_assert!(channel_id.value() < self.storage.get().channels.capacity());
            &self.storage.get().channels[channel_id.value()].state
        }
    }

    impl<Storage: DynamicStorage<SharedManagementData>> ZeroCopyReceiver for Receiver<Storage> {
        fn has_data(&self, channel_id: ChannelId) -> bool {
            debug_assert!(channel_id.value() < self.storage.get().channels.capacity());
            !self.storage.get().channels[channel_id.value()]
                .submission_queue
                .is_empty()
        }

        fn receive(
            &self,
            channel_id: ChannelId,
        ) -> Result<Option<PointerOffset>, ZeroCopyReceiveError> {
            debug_assert!(channel_id.value() < self.storage.get().channels.capacity());

            if *self.borrow_counter(channel_id) >= self.storage.get().max_borrowed_samples {
                fail!(from self, with ZeroCopyReceiveError::ReceiveWouldExceedMaxBorrowValue,
                "Unable to receive another sample since already {} samples were borrowed and this would exceed the max borrow value of {}.",
                    self.borrow_counter(channel_id), self.max_borrowed_chunks());
            }

            match unsafe {
                self.storage.get().channels[channel_id.value()]
                    .submission_queue
                    .pop()
            } {
                None => Ok(None),
                Some(v) => {
                    *self.borrow_counter(channel_id) += 1;
                    Ok(Some(PointerOffset::from_value(v)))
                }
            }
        }

        fn borrow_count(&self, channel_id: ChannelId) -> usize {
            *self.borrow_counter(channel_id)
        }

        fn release(
            &self,
            ptr: PointerOffset,
            channel_id: ChannelId,
        ) -> Result<(), ZeroCopyReleaseError> {
            debug_assert!(channel_id.value() < self.storage.get().channels.capacity());

            match unsafe {
                self.storage.get().channels[channel_id.value()]
                    .completion_queue
                    .push(ptr.as_value())
            } {
                true => {
                    *self.borrow_counter(channel_id) -= 1;
                    Ok(())
                }
                false => {
                    fail!(from self, with ZeroCopyReleaseError::RetrieveBufferFull,
                    "Unable to release pointer since the retrieve buffer is full.");
                }
            }
        }
    }

    #[derive(Debug)]
    pub struct Connection<Storage: DynamicStorage<SharedManagementData>> {
        _data: PhantomData<Storage>,
    }

    impl<Storage: DynamicStorage<SharedManagementData>> NamedConceptMgmt for Connection<Storage> {
        type Configuration = Configuration<Storage>;

        fn does_exist_cfg(
            name: &FileName,
            cfg: &Self::Configuration,
        ) -> Result<bool, crate::static_storage::file::NamedConceptDoesExistError> {
            Ok(fail!(from "ZeroCopyConnection::does_exist_cfg()",
                    when Storage::does_exist_cfg(name, &cfg.dynamic_storage_config),
                    "Failed to check if ZeroCopyConnection \"{}\" exists.",
                    name))
        }

        fn list_cfg(
            cfg: &Self::Configuration,
        ) -> Result<Vec<FileName>, crate::static_storage::file::NamedConceptListError> {
            Ok(fail!(from "ZeroCopyConnection::list_cfg()",
                    when Storage::list_cfg(&cfg.dynamic_storage_config),
                    "Failed to list all ZeroCopyConnections."))
        }

        unsafe fn remove_cfg(
            name: &FileName,
            cfg: &Self::Configuration,
        ) -> Result<bool, crate::static_storage::file::NamedConceptRemoveError> {
            Ok(fail!(from "ZeroCopyConnection::remove_cfg()",
                    when unsafe {Storage::remove_cfg(name, &cfg.dynamic_storage_config) },
                    "Failed to remove ZeroCopyConnection \"{}\".", name))
        }

        fn remove_path_hint(_value: &Path) -> Result<(), NamedConceptPathHintRemoveError> {
            Ok(())
        }
    }
    impl<Storage: DynamicStorage<SharedManagementData>> Connection<Storage> {
        fn remove_port(
            name: &FileName,
            config: &<Connection<Storage> as NamedConceptMgmt>::Configuration,
            msg: &str,
            port: State,
        ) -> Result<(), ZeroCopyPortRemoveError> {
            let origin = "Connection::open_storage()";
            match <<Storage as DynamicStorage<SharedManagementData>>::Builder<'_> as NamedConceptBuilder<
                    Storage>>::new(name)
                       .config(&config.dynamic_storage_config).open(AccessMode::ReadWrite) {
                           Ok(storage) => {
                               // RR R1：清理者只把合法 Open / hint 的 channel 真实 CAS 为私有
                               // ORPHANED（Acquire 读、AcqRel 成功）；已 CLOSED / ORPHANED 保持，
                               // 正常关闭不会被覆盖。之后才沿原路径移除端点位。
                               for channel in storage.get().channels.iter() {
                                   let _ = channel_state::mark_orphaned(&channel.state);
                               }
                               #[cfg(test)]
                               crate::zero_copy_connection::tests::remove_port_hook::after_orphan_cas_before_port_removal();
                               cleanup_shared_memory(&storage, port); Ok(())},
                           Err(DynamicStorageOpenError::InitializationNotYetFinalized) => {
                               match unsafe { Storage::remove_cfg(name, &config.dynamic_storage_config) } {
                                   Ok(_) => Ok(()),
                                   Err(NamedConceptRemoveError::InsufficientPermissions) => {
                                       fail!(from origin, with ZeroCopyPortRemoveError::InsufficientPermissions,
                                           "{msg} since the underlying dynamic storage has a different iceoryx2 version.");
                                   }
                                   Err(NamedConceptRemoveError::Interrupt) => {
                                       fail!(from origin, with ZeroCopyPortRemoveError::Interrupt,
                                           "{msg} since an interrupt signal was raised.");
                                   }
                                   Err(NamedConceptRemoveError::InternalError) => {
                                       fail!(from origin, with ZeroCopyPortRemoveError::InternalError,
                                           "{msg} due to an internal error.");
                                   }
                               }
                           }
                           Err(DynamicStorageOpenError::VersionMismatch) => {
                               fail!(from origin, with ZeroCopyPortRemoveError::VersionMismatch,
                                   "{msg} since the underlying dynamic storage has a different iceoryx2 version.");
                           }
                           Err(DynamicStorageOpenError::DoesNotExist) => {
                               fail!(from origin, with ZeroCopyPortRemoveError::DoesNotExist,
                                   "{msg} since the underlying dynamic storage does not exist.");
                           }
                           Err(DynamicStorageOpenError::InternalError) => {
                               fail!(from origin, with ZeroCopyPortRemoveError::InternalError,
                                   "{msg} due to an internal error.");
                           }
                       }
        }
    }

    impl<Storage: DynamicStorage<SharedManagementData>> ZeroCopyConnection for Connection<Storage> {
        type Sender = Sender<Storage>;
        type Builder = Builder<Storage>;
        type Receiver = Receiver<Storage>;

        unsafe fn remove_sender(
            name: &FileName,
            config: &Self::Configuration,
        ) -> Result<(), ZeroCopyPortRemoveError> {
            Self::remove_port(
                name,
                config,
                "Unable to remove forcefully the sender of the Zero Copy Connection",
                State::Sender,
            )
        }

        unsafe fn remove_receiver(
            name: &FileName,
            config: &Self::Configuration,
        ) -> Result<(), ZeroCopyPortRemoveError> {
            Self::remove_port(
                name,
                config,
                "Unable to remove forcefully the receiver of the Zero Copy Connection",
                State::Receiver,
            )
        }

        fn does_support_safe_overflow() -> bool {
            true
        }

        fn has_configurable_buffer_size() -> bool {
            true
        }
    }
}
