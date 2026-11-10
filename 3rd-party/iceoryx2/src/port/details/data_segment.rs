// Copyright (c) 2023 - 2024 Contributors to the Eclipse Foundation
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

use core::alloc::Layout;
use core::ptr::NonNull;

use iceoryx2_bb_derive_macros::ZeroCopySend;
use iceoryx2_bb_elementary::allocation_strategy::AllocationStrategy;
use iceoryx2_bb_elementary_traits::allocator::{
    Allocate, AllocationGrowError, ContentPlacement, Grow,
};
use iceoryx2_bb_elementary_traits::testing::abandonable::Abandonable;
use iceoryx2_bb_elementary_traits::zero_copy_send::ZeroCopySend;
use iceoryx2_bb_posix::file::AccessMode;
use iceoryx2_bb_system_types::file_name::FileName;
use iceoryx2_cal::{
    event::NamedConceptBuilder,
    resizable_shared_memory::*,
    shared_memory::{
        SharedMemory, SharedMemoryBuilder, SharedMemoryCreateError, SharedMemoryForPoolAllocator,
        SharedMemoryOpenError, ShmPointer,
    },
    shm_allocator::{
        self, AllocationError, PointerOffset, SegmentId, pool_allocator::PoolAllocator,
    },
};
use iceoryx2_log::fail;
use serde::{Deserialize, Serialize};

use crate::{
    config,
    service::{
        self,
        config_scheme::{data_segment_config, resizable_data_segment_config},
    },
};

/// Defines the data segment type of a zero copy capable sender port.
#[repr(C)]
#[derive(Debug, Clone, Copy, Eq, PartialEq, ZeroCopySend, Serialize, Deserialize)]
pub enum DataSegmentType {
    /// The data segment can be resized if no more memory is available.
    Dynamic,
    /// The data segment is allocated once. If it is out-of-memory no reallocation will occur.
    Static,
}

impl DataSegmentType {
    pub(crate) fn new_from_allocation_strategy(v: AllocationStrategy) -> Self {
        match v {
            AllocationStrategy::Static => DataSegmentType::Static,
            _ => DataSegmentType::Dynamic,
        }
    }
}

#[derive(Debug)]
enum MemoryType<Service: service::Service> {
    Static(Service::SharedMemory),
    Dynamic(Service::ResizableSharedMemory),
}

#[derive(Debug)]
pub(crate) struct DataSegment<Service: service::Service> {
    memory: MemoryType<Service>,
}

impl<Service: service::Service> Abandonable for DataSegment<Service> {
    unsafe fn abandon_in_place(mut this: NonNull<Self>) {
        let this = unsafe { this.as_mut() };
        match &mut this.memory {
            MemoryType::Static(shm) => {
                unsafe { Service::SharedMemory::abandon_in_place(NonNull::from_mut(shm)) };
            }
            MemoryType::Dynamic(shm) => unsafe {
                Service::ResizableSharedMemory::abandon_in_place(NonNull::from_mut(shm));
            },
        }
    }
}

/// RR R2：`UniqueIndexSet` 只用 24 位保存 head / borrowed 计数，池 bucket 数须 `< 2^24 − 1`。
pub(crate) const MAX_POOL_BUCKETS: usize = (1 << 24) - 2;

/// RR R2：静态段计划的 typed 失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StaticSegmentPlanError {
    /// 字节数 / 管理区 / 总量算术溢出或布局非法。
    Overflow,
    /// 池 bucket 数超过底层 24 位界。
    ExceedsMaxSupportedChunks,
}

impl<Service: service::Service> Grow<ShmPointer> for DataSegment<Service> {
    unsafe fn grow(
        &self,
        ptr: ShmPointer,
        old_layout: Layout,
        new_layout: Layout,
        content_placement: ContentPlacement,
    ) -> Result<ShmPointer, AllocationGrowError> {
        let msg = "Unable to grow memory cell from the data segment";
        match &self.memory {
            MemoryType::Static(memory) => Ok(fail!(from self,
                    when unsafe { memory.grow(ptr, old_layout, new_layout, content_placement) },
                    "{msg}.")),
            MemoryType::Dynamic(memory) => Ok(fail!(from self,
                    when unsafe { memory.grow(ptr, old_layout, new_layout, content_placement) },
                    "{msg}.")),
        }
    }
}

impl<Service: service::Service> DataSegment<Service> {
    /// RR R2：受检的静态段计划，返回 payload 字节数 `M = s * N + a − 1`。在此之前核：
    /// 池 bucket 数上界 `K_max = floor(M / align_up(s, a))` 不超过 `UniqueIndexSet` 的 24 位界
    /// （合法最大 `2^24 − 2`）、池管理区 `G = size_of::<u32>() * (floor(M / s) + 1) + align_of::<u32>() − 1`
    /// 与总量 `M + G` 均可表示且不超过 `isize::MAX`。任一步失败 typed 返回，不申请任何资源。
    pub(crate) fn checked_static_segment_plan(
        chunk_layout: Layout,
        number_of_chunks: usize,
    ) -> Result<usize, StaticSegmentPlanError> {
        let size = chunk_layout.size();
        let align = chunk_layout.align();
        if size == 0 {
            return Err(StaticSegmentPlanError::Overflow);
        }
        let payload_size = size
            .checked_mul(number_of_chunks)
            .and_then(|v| v.checked_add(align - 1))
            .ok_or(StaticSegmentPlanError::Overflow)?;
        let bucket_size = size
            .checked_next_multiple_of(align)
            .ok_or(StaticSegmentPlanError::Overflow)?;
        let max_buckets = payload_size / bucket_size;
        if max_buckets > MAX_POOL_BUCKETS {
            return Err(StaticSegmentPlanError::ExceedsMaxSupportedChunks);
        }
        let min_required_buckets = payload_size / size;
        let management_size = core::mem::size_of::<u32>()
            .checked_mul(
                min_required_buckets
                    .checked_add(1)
                    .ok_or(StaticSegmentPlanError::Overflow)?,
            )
            .and_then(|v| v.checked_add(core::mem::align_of::<u32>() - 1))
            .ok_or(StaticSegmentPlanError::Overflow)?;
        let total = payload_size
            .checked_add(management_size)
            .ok_or(StaticSegmentPlanError::Overflow)?;
        if total > isize::MAX as usize {
            return Err(StaticSegmentPlanError::Overflow);
        }
        Ok(payload_size)
    }

    pub(crate) fn create_static_segment(
        segment_name: &FileName,
        chunk_layout: Layout,
        global_config: &config::Config,
        number_of_chunks: usize,
    ) -> Result<Self, SharedMemoryCreateError> {
        let allocator_config = shm_allocator::pool_allocator::Config {
            bucket_layout: chunk_layout,
        };
        let msg = "Unable to create the static data segment since the underlying shared memory could not be created.";
        let origin = "DataSegment::create_static_segment()";

        let segment_config = data_segment_config::<Service>(global_config);
        // RR R2：字节数按受检计划计算（单一算式来源）；RR 构造入口已先行核过，这里失败即 typed
        // 而非回绕。
        let segment_size = match Self::checked_static_segment_plan(chunk_layout, number_of_chunks) {
            Ok(size) => size,
            Err(e) => {
                fail!(from origin, with SharedMemoryCreateError::InternalError,
                    "{msg} The segment plan for {number_of_chunks} chunks of {chunk_layout:?} is not representable ({e:?}).");
            }
        };
        let memory = fail!(from origin,
                                when <<Service::SharedMemory as SharedMemory<PoolAllocator>>::Builder as NamedConceptBuilder<
                                Service::SharedMemory,
                                    >>::new(segment_name)
                                    .config(&segment_config)
                                    .size(segment_size)
                                    .create(&allocator_config),
                                "{msg}");

        Ok(Self {
            memory: MemoryType::Static(memory),
        })
    }

    pub(crate) fn create_dynamic_segment(
        segment_name: &FileName,
        chunk_layout: Layout,
        global_config: &config::Config,
        number_of_chunks: usize,
        allocation_strategy: AllocationStrategy,
    ) -> Result<Self, SharedMemoryCreateError> {
        let msg = "Unable to create the dynamic data segment since the underlying shared memory could not be created.";
        let origin = "DataSegment::create_dynamic_segment()";

        let segment_config = resizable_data_segment_config::<Service>(global_config);
        let memory = fail!(from origin,
                    when <<Service::ResizableSharedMemory as ResizableSharedMemory<
                        PoolAllocator,
                        Service::SharedMemory,
                    >>::MemoryBuilder as NamedConceptBuilder<Service::ResizableSharedMemory>>::new(
                        segment_name,
                    )
                    .config(&segment_config)
                    .max_number_of_chunks_hint(number_of_chunks)
                    .max_chunk_layout_hint(chunk_layout)
                    .allocation_strategy(allocation_strategy)
                    .create(),
                    "{msg}");

        Ok(Self {
            memory: MemoryType::Dynamic(memory),
        })
    }

    pub(crate) fn allocate(&self, layout: Layout) -> Result<ShmPointer, AllocationError> {
        let msg = "Unable to allocate memory from the data segment";
        match &self.memory {
            MemoryType::Static(memory) => Ok(fail!(from self, when memory.allocate(layout),
                                            "{msg}.")),
            MemoryType::Dynamic(memory) => {
                Ok(fail!(from self, when memory.allocate(layout), "{msg}."))
            }
        }
    }

    pub(crate) fn allocation_strategy(&self) -> AllocationStrategy {
        match &self.memory {
            MemoryType::Static(_) => AllocationStrategy::Static,
            MemoryType::Dynamic(memory) => memory.allocation_strategy(),
        }
    }

    pub(crate) unsafe fn deallocate_bucket(&self, offset: PointerOffset) {
        unsafe {
            match &self.memory {
                MemoryType::Static(memory) => memory.deallocate_bucket(offset),
                MemoryType::Dynamic(memory) => memory.deallocate_bucket(offset),
            }
        }
    }

    pub(crate) fn bucket_size(&self, segment_id: SegmentId) -> usize {
        match &self.memory {
            MemoryType::Static(memory) => memory.bucket_size(),
            MemoryType::Dynamic(memory) => memory.bucket_size(segment_id),
        }
    }

    pub(crate) fn max_number_of_segments(data_segment_type: DataSegmentType) -> u8 {
        match data_segment_type {
            DataSegmentType::Static => 1,
            DataSegmentType::Dynamic => {
                (Service::ResizableSharedMemory::max_number_of_reallocations()) as u8
            }
        }
    }
}

#[derive(Debug)]
enum MemoryViewType<Service: service::Service> {
    Static(Service::SharedMemory),
    Dynamic(
        <Service::ResizableSharedMemory as ResizableSharedMemory<
            PoolAllocator,
            Service::SharedMemory,
        >>::View,
    ),
}

#[derive(Debug)]
pub(crate) struct DataSegmentView<Service: service::Service> {
    memory: MemoryViewType<Service>,
}

impl<Service: service::Service> Abandonable for DataSegmentView<Service> {
    unsafe fn abandon_in_place(mut this: NonNull<Self>) {
        let this = unsafe { this.as_mut() };
        match &mut this.memory {
            MemoryViewType::Dynamic(shm) => unsafe {
                <Service::ResizableSharedMemory as ResizableSharedMemory<
                    PoolAllocator,
                    Service::SharedMemory,
                >>::View::abandon_in_place(NonNull::from_mut(shm))
            },
            MemoryViewType::Static(shm) => unsafe {
                Service::SharedMemory::abandon_in_place(NonNull::from_mut(shm));
            },
        }
    }
}

impl<Service: service::Service> DataSegmentView<Service> {
    pub(crate) fn open_static_segment(
        segment_name: &FileName,
        global_config: &config::Config,
    ) -> Result<Self, SharedMemoryOpenError> {
        let origin = "DataSegment::open()";
        let msg =
            "Unable to open data segment since the underlying shared memory could not be opened.";

        let segment_config = data_segment_config::<Service>(global_config);
        let memory = fail!(from origin,
                            when <Service::SharedMemory as SharedMemory<PoolAllocator>>::
                                Builder::new(segment_name)
                                .config(&segment_config)
                                .timeout(global_config.global.creation_timeout)
                                .open(AccessMode::Read),
                            "{msg}");

        Ok(Self {
            memory: MemoryViewType::Static(memory),
        })
    }

    pub(crate) fn open_dynamic_segment(
        segment_name: &FileName,
        global_config: &config::Config,
    ) -> Result<Self, SharedMemoryOpenError> {
        let origin = "DataSegment::open()";
        let msg =
            "Unable to open data segment since the underlying shared memory could not be opened.";

        let segment_config = resizable_data_segment_config::<Service>(global_config);
        let memory = fail!(from origin,
                    when <<Service::ResizableSharedMemory as ResizableSharedMemory<
                        PoolAllocator,
                        Service::SharedMemory,
                    >>::ViewBuilder as NamedConceptBuilder<Service::ResizableSharedMemory>>::new(
                        segment_name,
                    )
                    .config(&segment_config)
                    .open(AccessMode::Read),
                    "{msg}");

        Ok(Self {
            memory: MemoryViewType::Dynamic(memory),
        })
    }

    pub(crate) fn register_and_translate_offset(
        &self,
        offset: PointerOffset,
    ) -> Result<usize, SharedMemoryOpenError> {
        match &self.memory {
            MemoryViewType::Static(memory) => Ok(offset.offset() + memory.payload_start_address()),
            MemoryViewType::Dynamic(memory) => unsafe {
                match memory.register_and_translate_offset(offset) {
                    Ok(ptr) => Ok(ptr as usize),
                    Err(e) => {
                        fail!(from self, with e,
                            "Failed to register and translate pointer due to a failure while opening the corresponding shared memory segment ({:?}).",
                            e);
                    }
                }
            },
        }
    }

    pub(crate) unsafe fn unregister_offset(&self, offset: PointerOffset) {
        unsafe {
            if let MemoryViewType::Dynamic(memory) = &self.memory {
                memory.unregister_offset(offset);
            }
        }
    }

    pub(crate) fn is_dynamic(&self) -> bool {
        matches!(&self.memory, MemoryViewType::Dynamic(_))
    }
}
