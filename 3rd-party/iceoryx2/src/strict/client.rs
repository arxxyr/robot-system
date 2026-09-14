// RR R2（robot-system 本地补丁）：受检的严格单 server client。
//
// 许可：与所在 crate 相同（MIT OR Apache-2.0）。

use core::{fmt::Debug, marker::PhantomData, mem::MaybeUninit};

use alloc::format;
use iceoryx2_bb_concurrency::atomic::{AtomicU64, Ordering};
use iceoryx2_bb_elementary::CallbackProgression;
use iceoryx2_bb_elementary_traits::zero_copy_send::ZeroCopySend;
use iceoryx2_cal::arc_sync_policy::ArcSyncPolicy;
use iceoryx2_cal::shm_allocator::{AllocationStrategy, PointerOffset};
use iceoryx2_cal::zero_copy_connection::{
    ChannelId, EnqueueIssue, StrictChannelStatus, TrySendReceipt,
};
use iceoryx2_log::{fail, fatal_panic};

use crate::active_request::RequestId;
use crate::identifiers::UniqueClientId;
use crate::port::ReceiveError;
use crate::port::client::{Client, ClientSharedState, StrictDetached, StrictSlot};
use crate::port::details::chunk::ChunkMut;
use crate::port::details::data_segment::DataSegmentType;
use crate::port::details::receiver::{RetentionPolicy, RoutePin};
use crate::prelude::{BackpressureStrategy, PortFactory};
use crate::raw_sample::RawSampleMut;
use crate::response::Response;
use crate::service;
use crate::service::header::request_response::RequestHeader as RequestHeaderInternal;
use crate::service::header::request_response::ResponseHeader as ResponseHeaderInternal;
use crate::service::port_factory::client::PortFactoryClient;

use super::{StrictCreateError, StrictLoanError, StrictSendRefusal};

/// strict 请求序号合法域 `1..=2^62−1`；0 留给 CAL 的 OPEN 初值，不作请求序号。
const STRICT_REQUEST_ID_LIMIT: u64 = 1u64 << 62;

/// 所有请求经 channel 0 的请求缓冲投递；只有响应按 channel 区分。
const REQUEST_CHANNEL_ID: ChannelId = ChannelId::new(0);

/// 发送前 loan 期间持有的本地原件（无 Arc、不可 Clone）。
#[derive(Debug, Clone, Copy)]
struct LoanParts {
    offset: PointerOffset,
    sample_size: usize,
    channel_id: ChannelId,
    request_id: RequestId,
}

/// 严格单 server client：只提供受检的 loan / send 入口，不暴露普通多目标 API。
#[derive(Debug)]
pub struct StrictClient<
    Service: service::Service,
    RequestPayload: Debug + ZeroCopySend + ?Sized,
    RequestHeader: Debug + ZeroCopySend,
    ResponsePayload: Debug + ZeroCopySend + ?Sized,
    ResponseHeader: Debug + ZeroCopySend,
> {
    client: Client<Service, RequestPayload, RequestHeader, ResponsePayload, ResponseHeader>,
    /// 下一个待分配的 strict 请求序号；不回绕。
    next_request_id: AtomicU64,
}

impl<
    Service: service::Service,
    RequestPayload: Debug + ZeroCopySend + ?Sized,
    RequestHeader: Debug + ZeroCopySend,
    ResponsePayload: Debug + ZeroCopySend + ?Sized,
    ResponseHeader: Debug + ZeroCopySend,
> StrictClient<Service, RequestPayload, RequestHeader, ResponsePayload, ResponseHeader>
{
    pub(crate) fn new(
        factory: PortFactoryClient<
            Service,
            RequestPayload,
            RequestHeader,
            ResponsePayload,
            ResponseHeader,
        >,
    ) -> Result<Self, StrictCreateError> {
        let origin = format!("{factory:?}");
        let static_config = factory.factory.static_config();
        if static_config.max_servers() != 1 {
            fail!(from origin, with StrictCreateError::MaxServersNotOne { actual: static_config.max_servers() },
                "Unable to create strict client since the service allows {} servers instead of exactly one.",
                static_config.max_servers());
        }
        if factory.config.allocation_strategy != AllocationStrategy::Static {
            fail!(from origin, with StrictCreateError::AllocationNotStatic,
                "Unable to create strict client since the allocation strategy is not Static.");
        }
        if factory.config.backpressure_strategy != BackpressureStrategy::DiscardData {
            fail!(from origin, with StrictCreateError::BackpressureNotDiscardData,
                "Unable to create strict client since the backpressure strategy is not DiscardData.");
        }
        if factory.backpressure_handler.is_some() {
            fail!(from origin, with StrictCreateError::CustomBackpressureHandler,
                "Unable to create strict client since a custom backpressure handler is set.");
        }
        if static_config.has_safe_overflow_for_requests() {
            fail!(from origin, with StrictCreateError::RequestOverflowEnabled,
                "Unable to create strict client since the service enables safe overflow for requests.");
        }

        // 容量 / 布局 / 固定表均在 `Client::new` 内按 override 后真实 N 受检计算与可失败预留，
        // typed 经 `ClientCreateError::{CapacityOverflow, InsufficientMemory}` 返回。
        let client = match Client::new(factory, RetentionPolicy::StrictPinned) {
            Ok(client) => client,
            Err(e) => {
                fail!(from origin, with StrictCreateError::Client(e),
                    "Unable to create strict client since the underlying client could not be created ({e:?}).");
            }
        };

        Ok(Self {
            client,
            next_request_id: AtomicU64::new(1),
        })
    }

    /// 完整原 client id。
    pub fn id(&self) -> UniqueClientId {
        self.client.id()
    }

    /// 只读：当前 Detached 槽数（维护 / 退出处置的观测口，不参与判定）。
    pub fn number_of_detached_requests(&self) -> usize {
        self.client
            .client_shared_state
            .lock()
            .strict_slots()
            .iter()
            .filter(|slot| matches!(slot, StrictSlot::Detached(_)))
            .count()
    }

    /// 只读：当前被占用的固定槽数（Live + Detached）。
    pub fn number_of_pinned_routes(&self) -> usize {
        self.client
            .client_shared_state
            .lock()
            .strict_slots()
            .iter()
            .filter(|slot| !matches!(slot, StrictSlot::Free))
            .count()
    }

    /// 只读：当前 active 请求计数（Live + Detached）。
    pub fn number_of_active_requests(&self) -> usize {
        self.client
            .client_shared_state
            .lock()
            .active_request_counter
            .load(Ordering::Relaxed)
    }

    /// 只读：response channel 总数（= override 后真实 N = 固定槽数）。
    pub fn number_of_channels(&self) -> usize {
        self.client.client_shared_state.lock().strict_slots().len()
    }

    /// 执行一轮 Detached 正常维护（同 shared 锁内；loan / send / receive 也会执行）。
    pub fn maintain(&self) {
        self.client.client_shared_state.lock().maintain_detached();
    }

    /// 仅测试：共享状态句柄。
    #[cfg(test)]
    pub(crate) fn shared_for_test(
        &self,
    ) -> &Service::ArcThreadSafetyPolicy<ClientSharedState<Service>> {
        &self.client.client_shared_state
    }

    /// 下一个 strict 请求序号；最后合法值 `2^62−1` 可分配，之后才 typed 耗尽。
    /// 只在持有 shared 锁时调用：检查与消费构成同一锁事务，并发 loan 不会拿到同一序号。
    fn peek_request_id_locked(&self) -> Result<RequestId, StrictLoanError> {
        let next = self.next_request_id.load(Ordering::Relaxed);
        if next >= STRICT_REQUEST_ID_LIMIT {
            return Err(StrictLoanError::RequestIdExhausted);
        }
        Ok(RequestId::new(next).expect("next < 2^62 is a legal ChannelState"))
    }

    /// 只在持有 shared 锁且 `peek_request_id_locked` 成功后调用。
    fn consume_request_id_locked(&self) {
        self.next_request_id.fetch_add(1, Ordering::Relaxed);
    }

    /// 仅测试：把下一个序号设到指定值，用于最后合法值 / 耗尽断言。
    #[cfg(test)]
    pub(crate) fn set_next_request_id_for_test(&self, value: u64) {
        let _shared = self.client.client_shared_state.lock();
        self.next_request_id.store(value, Ordering::Relaxed);
    }

    /// 共用的 loan 内核（整个过程持同一 shared 锁）：先核序号可用，再分配 chunk，再取 channel，
    /// 最后才消费序号并写内部 header；任一步失败回滚已分配原件，序号不消费。
    /// 返回的 chunk 尚无 RAII：调用方须先建立 `StrictRequestMut` 守卫，再执行可能 panic 的用户
    /// header 初始化。
    fn loan_parts(
        &self,
        number_of_elements: usize,
    ) -> Result<(LoanParts, ChunkMut), StrictLoanError> {
        let shared = self.client.client_shared_state.lock();
        let request_id = self.peek_request_id_locked()?;
        shared.maintain_detached();

        let layout = shared.request_sender.sample_layout(number_of_elements);
        let chunk = match shared.request_sender.allocate(layout) {
            Ok(chunk) => chunk,
            Err(e) => return Err(StrictLoanError::Loan(e)),
        };

        let channel_id = match unsafe { &mut *shared.available_channel_ids.get() }.pop() {
            Some(channel_id) => channel_id,
            None => {
                // 回滚 chunk：本地引用与 loan 计数。
                shared.request_sender.return_loaned_sample(chunk.offset);
                return Err(StrictLoanError::NoChannelAvailable);
            }
        };
        if !matches!(shared.strict_slots()[channel_id.value()], StrictSlot::Free) {
            fatal_panic!(from self, "This should never happen! A channel from the pool has a non-free strict slot.");
        }

        self.consume_request_id_locked();
        let header_ptr: *mut RequestHeaderInternal = chunk.header.cast();
        unsafe {
            header_ptr.write(RequestHeaderInternal {
                node_id: *shared.request_sender.shared_node.id(),
                client_id: self.id(),
                channel_id,
                request_id,
                number_of_elements: number_of_elements as _,
            });
        }
        Ok((
            LoanParts {
                offset: chunk.offset,
                sample_size: chunk.size,
                channel_id,
                request_id,
            },
            chunk,
        ))
    }
}

impl<
    Service: service::Service,
    RequestPayload: Debug + ZeroCopySend,
    RequestHeader: Default + Debug + ZeroCopySend,
    ResponsePayload: Debug + ZeroCopySend + ?Sized,
    ResponseHeader: Debug + ZeroCopySend,
> StrictClient<Service, RequestPayload, RequestHeader, ResponsePayload, ResponseHeader>
{
    /// 受检 loan（typed 载荷）。
    #[allow(clippy::type_complexity)]
    pub fn loan_uninit(
        &self,
    ) -> Result<
        StrictRequestMutUninit<
            Service,
            MaybeUninit<RequestPayload>,
            RequestHeader,
            ResponsePayload,
            ResponseHeader,
        >,
        StrictLoanError,
    > {
        let (parts, chunk) = self.loan_parts(1)?;
        let user_header_ptr: *mut RequestHeader = chunk.user_header.cast();
        let ptr = unsafe {
            RawSampleMut::<RequestHeaderInternal, RequestHeader, MaybeUninit<RequestPayload>>::new_unchecked(
                chunk.header.cast(),
                user_header_ptr,
                chunk.payload.cast(),
            )
        };
        // 先建立 loan RAII（Drop 归还 loan / channel），再执行可能 panic 的用户 header 初始化。
        let request = StrictRequestMut {
            parts: Some(parts),
            ptr,
            shared: self.client.client_shared_state.clone(),
            _response_payload: PhantomData,
            _response_header: PhantomData,
        };
        unsafe { user_header_ptr.write(RequestHeader::default()) };
        Ok(StrictRequestMutUninit { request })
    }
}

impl<
    Service: service::Service,
    RequestPayload: Debug + ZeroCopySend + 'static,
    RequestHeader: Default + Debug + ZeroCopySend,
    ResponsePayload: Debug + ZeroCopySend + ?Sized,
    ResponseHeader: Debug + ZeroCopySend,
> StrictClient<Service, [RequestPayload], RequestHeader, ResponsePayload, ResponseHeader>
{
    /// 受检 loan（切片载荷）；Static 分配下 `slice_len` 不得超过 `initial_max_slice_len`。
    #[allow(clippy::type_complexity)]
    pub fn loan_slice_uninit(
        &self,
        slice_len: usize,
    ) -> Result<
        StrictRequestMutUninit<
            Service,
            [MaybeUninit<RequestPayload>],
            RequestHeader,
            ResponsePayload,
            ResponseHeader,
        >,
        StrictLoanError,
    > {
        let max_slice_len = self
            .client
            .client_shared_state
            .lock()
            .config
            .initial_max_slice_len;
        if max_slice_len < slice_len {
            fail!(from self, with StrictLoanError::Loan(crate::port::LoanError::ExceedsMaxLoanSize),
                "Unable to loan slice with {} elements since it would exceed the max supported slice length of {}.",
                slice_len, max_slice_len);
        }
        let (parts, chunk) = self.loan_parts(slice_len)?;
        let user_header_ptr: *mut RequestHeader = chunk.user_header.cast();
        let ptr = unsafe {
            RawSampleMut::<RequestHeaderInternal, RequestHeader, [MaybeUninit<RequestPayload>]>::new_unchecked(
                chunk.header.cast(),
                user_header_ptr,
                core::ptr::slice_from_raw_parts_mut(chunk.payload.cast(), slice_len),
            )
        };
        // 先建立 loan RAII，再执行可能 panic 的用户 header 初始化。
        let request = StrictRequestMut {
            parts: Some(parts),
            ptr,
            shared: self.client.client_shared_state.clone(),
            _response_payload: PhantomData,
            _response_header: PhantomData,
        };
        unsafe { user_header_ptr.write(RequestHeader::default()) };
        Ok(StrictRequestMutUninit { request })
    }
}

/// 未初始化的 strict 请求（持有已建立的 loan RAII）。
pub struct StrictRequestMutUninit<
    Service: service::Service,
    RequestPayload: Debug + ZeroCopySend + ?Sized,
    RequestHeader: Debug + ZeroCopySend,
    ResponsePayload: Debug + ZeroCopySend + ?Sized,
    ResponseHeader: Debug + ZeroCopySend,
> {
    request:
        StrictRequestMut<Service, RequestPayload, RequestHeader, ResponsePayload, ResponseHeader>,
}

impl<
    Service: service::Service,
    RequestPayload: Debug + ZeroCopySend + ?Sized,
    RequestHeader: Debug + ZeroCopySend,
    ResponsePayload: Debug + ZeroCopySend + ?Sized,
    ResponseHeader: Debug + ZeroCopySend,
> Debug
    for StrictRequestMutUninit<
        Service,
        RequestPayload,
        RequestHeader,
        ResponsePayload,
        ResponseHeader,
    >
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "StrictRequestMutUninit {{ request: {:?} }}",
            self.request
        )
    }
}

impl<
    Service: service::Service,
    RequestPayload: Debug + ZeroCopySend + ?Sized,
    RequestHeader: Debug + ZeroCopySend,
    ResponsePayload: Debug + ZeroCopySend + ?Sized,
    ResponseHeader: Debug + ZeroCopySend,
> StrictRequestMutUninit<Service, RequestPayload, RequestHeader, ResponsePayload, ResponseHeader>
{
    /// 内部请求 header（node / client id / channel / 请求序号 / 元素数）。
    pub fn header(&self) -> &RequestHeaderInternal {
        self.request.header()
    }

    /// 可变用户 header。
    pub fn user_header_mut(&mut self) -> &mut RequestHeader {
        self.request.user_header_mut()
    }

    /// 可变载荷。
    pub fn payload_mut(&mut self) -> &mut RequestPayload {
        self.request.payload_mut()
    }
}

impl<
    Service: service::Service,
    RequestPayload: Debug + ZeroCopySend + Sized,
    RequestHeader: Debug + ZeroCopySend,
    ResponsePayload: Debug + ZeroCopySend + ?Sized,
    ResponseHeader: Debug + ZeroCopySend,
>
    StrictRequestMutUninit<
        Service,
        MaybeUninit<RequestPayload>,
        RequestHeader,
        ResponsePayload,
        ResponseHeader,
    >
{
    /// 写入载荷并标记为已初始化。
    pub fn write_payload(
        mut self,
        value: RequestPayload,
    ) -> StrictRequestMut<Service, RequestPayload, RequestHeader, ResponsePayload, ResponseHeader>
    {
        self.payload_mut().write(value);
        unsafe { self.assume_init() }
    }

    /// # Safety
    ///
    /// 调用方须已初始化整个载荷。
    pub unsafe fn assume_init(
        self,
    ) -> StrictRequestMut<Service, RequestPayload, RequestHeader, ResponsePayload, ResponseHeader>
    {
        // 按字段转移原所有权（loan 原件、shared 引用），只把已初始化的 payload 指针转换为
        // 已初始化类型；不对外层泛型结构整体位拷贝。
        let StrictRequestMutUninit { request } = self;
        let mut request = core::mem::ManuallyDrop::new(request);
        let parts = request.parts.take();
        let shared = unsafe { core::ptr::read(&request.shared) };
        let ptr = unsafe {
            RawSampleMut::new_unchecked(
                request.ptr.header_ptr(),
                request.ptr.user_header_ptr(),
                request.ptr.payload_ptr().cast::<RequestPayload>(),
            )
        };
        StrictRequestMut {
            parts,
            ptr,
            shared,
            _response_payload: PhantomData,
            _response_header: PhantomData,
        }
    }
}

impl<
    Service: service::Service,
    RequestPayload: Debug + ZeroCopySend,
    RequestHeader: Debug + ZeroCopySend,
    ResponsePayload: Debug + ZeroCopySend + ?Sized,
    ResponseHeader: Debug + ZeroCopySend,
>
    StrictRequestMutUninit<
        Service,
        [MaybeUninit<RequestPayload>],
        RequestHeader,
        ResponsePayload,
        ResponseHeader,
    >
{
    /// # Safety
    ///
    /// 调用方须已初始化整个切片。
    pub unsafe fn assume_init(
        self,
    ) -> StrictRequestMut<Service, [RequestPayload], RequestHeader, ResponsePayload, ResponseHeader>
    {
        let StrictRequestMutUninit { request } = self;
        let mut request = core::mem::ManuallyDrop::new(request);
        let parts = request.parts.take();
        let shared = unsafe { core::ptr::read(&request.shared) };
        let payload = request.ptr.payload_ptr();
        let len = payload.len();
        let ptr = unsafe {
            RawSampleMut::new_unchecked(
                request.ptr.header_ptr(),
                request.ptr.user_header_ptr(),
                core::ptr::slice_from_raw_parts_mut(
                    (payload as *mut MaybeUninit<RequestPayload>).cast::<RequestPayload>(),
                    len,
                ),
            )
        };
        StrictRequestMut {
            parts,
            ptr,
            shared,
            _response_payload: PhantomData,
            _response_header: PhantomData,
        }
    }

    /// 用初始化函数逐元素写入并标记为已初始化。
    pub fn write_from_fn<F: FnMut(usize) -> RequestPayload>(
        mut self,
        mut initializer: F,
    ) -> StrictRequestMut<Service, [RequestPayload], RequestHeader, ResponsePayload, ResponseHeader>
    {
        for (i, element) in self.payload_mut().iter_mut().enumerate() {
            element.write(initializer(i));
        }
        unsafe { self.assume_init() }
    }
}

impl<
    Service: service::Service,
    RequestPayload: Debug + Copy + ZeroCopySend,
    RequestHeader: Debug + ZeroCopySend,
    ResponsePayload: Debug + ZeroCopySend + ?Sized,
    ResponseHeader: Debug + ZeroCopySend,
>
    StrictRequestMutUninit<
        Service,
        [MaybeUninit<RequestPayload>],
        RequestHeader,
        ResponsePayload,
        ResponseHeader,
    >
{
    /// 从切片逐元素写入并标记为已初始化（以载荷切片长度为准）。
    pub fn write_from_slice(
        mut self,
        value: &[RequestPayload],
    ) -> StrictRequestMut<Service, [RequestPayload], RequestHeader, ResponsePayload, ResponseHeader>
    {
        // 长度必须完全一致（沿 `copy_from_slice` 语义 panic）；panic 时由已建立的 loan RAII 归还。
        assert_eq!(
            self.payload_mut().len(),
            value.len(),
            "write_from_slice requires a slice of exactly the loaned length"
        );
        for (element, v) in self.payload_mut().iter_mut().zip(value) {
            element.write(*v);
        }
        unsafe { self.assume_init() }
    }
}

/// 已初始化、尚未发送的 strict 请求。未发送 Drop 归原 loan；发送成功后原件只转给 Pending。
pub struct StrictRequestMut<
    Service: service::Service,
    RequestPayload: Debug + ZeroCopySend + ?Sized,
    RequestHeader: Debug + ZeroCopySend,
    ResponsePayload: Debug + ZeroCopySend + ?Sized,
    ResponseHeader: Debug + ZeroCopySend,
> {
    parts: Option<LoanParts>,
    ptr: RawSampleMut<RequestHeaderInternal, RequestHeader, RequestPayload>,
    shared: Service::ArcThreadSafetyPolicy<ClientSharedState<Service>>,
    _response_payload: PhantomData<ResponsePayload>,
    _response_header: PhantomData<ResponseHeader>,
}

impl<
    Service: service::Service,
    RequestPayload: Debug + ZeroCopySend + ?Sized,
    RequestHeader: Debug + ZeroCopySend,
    ResponsePayload: Debug + ZeroCopySend + ?Sized,
    ResponseHeader: Debug + ZeroCopySend,
> Debug
    for StrictRequestMut<Service, RequestPayload, RequestHeader, ResponsePayload, ResponseHeader>
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "StrictRequestMut {{ parts: {:?} }}", self.parts)
    }
}

impl<
    Service: service::Service,
    RequestPayload: Debug + ZeroCopySend + ?Sized,
    RequestHeader: Debug + ZeroCopySend,
    ResponsePayload: Debug + ZeroCopySend + ?Sized,
    ResponseHeader: Debug + ZeroCopySend,
> Drop
    for StrictRequestMut<Service, RequestPayload, RequestHeader, ResponsePayload, ResponseHeader>
{
    fn drop(&mut self) {
        // 未发送：归还原 loan（本地引用、loan 计数、channel 池位置）。发送成功后 parts 已按值转出。
        if let Some(parts) = self.parts.take() {
            let shared = self.shared.lock();
            shared.request_sender.return_loaned_sample(parts.offset);
            if !unsafe { &mut *shared.available_channel_ids.get() }.push(parts.channel_id) {
                fatal_panic!(from self, "This should never happen! The channel id could not be returned.");
            }
        }
    }
}

impl<
    Service: service::Service,
    RequestPayload: Debug + ZeroCopySend + ?Sized,
    RequestHeader: Debug + ZeroCopySend,
    ResponsePayload: Debug + ZeroCopySend + ?Sized,
    ResponseHeader: Debug + ZeroCopySend,
> StrictRequestMut<Service, RequestPayload, RequestHeader, ResponsePayload, ResponseHeader>
{
    /// 内部请求 header（node / client id / channel / 请求序号 / 元素数）。
    pub fn header(&self) -> &RequestHeaderInternal {
        self.ptr.as_header_ref()
    }

    /// 用户 header。
    pub fn user_header(&self) -> &RequestHeader {
        self.ptr.as_user_header_ref()
    }

    /// 可变用户 header。
    pub fn user_header_mut(&mut self) -> &mut RequestHeader {
        self.ptr.as_user_header_mut()
    }

    /// 载荷。
    pub fn payload(&self) -> &RequestPayload {
        self.ptr.as_payload_ref()
    }

    /// 可变载荷。
    pub fn payload_mut(&mut self) -> &mut RequestPayload {
        self.ptr.as_payload_mut()
    }

    /// 本请求的 strict 序号。
    pub fn request_id(&self) -> RequestId {
        self.header().request_id
    }

    /// 本请求占用的 response channel。
    pub fn channel_id(&self) -> ChannelId {
        self.header().channel_id
    }

    /// 仅测试：对未发布且唯一持有的原 loan 注入畸形内部身份，验证真实接收校验。
    #[cfg(test)]
    pub(crate) fn set_header_client_id_for_test(&mut self, client_id: UniqueClientId) {
        unsafe {
            self.ptr.header_ptr().as_mut().expect("原 header").client_id = client_id;
        }
    }

    /// 仅故障测试：未发送原件的 offset。
    #[cfg(all(test, feature = "r2-test-fixtures"))]
    pub(crate) fn offset_for_test(&self) -> PointerOffset {
        self.parts.as_ref().expect("未发送原件").offset
    }

    #[cfg(all(test, feature = "r2-test-fixtures"))]
    pub(crate) fn sample_size_for_test(&self) -> usize {
        self.parts.as_ref().expect("未发送原件").sample_size
    }

    /// 唯一严格发送：同一 shared 锁内维护、更新、预留原槽/pin、prepare，然后唯一 enqueue。
    /// 入队前拒绝完整返回原 request；发布后按值转移原件到 Pending。
    #[allow(clippy::type_complexity)]
    pub fn send(
        mut self,
    ) -> Result<
        StrictPendingResponse<
            Service,
            RequestPayload,
            RequestHeader,
            ResponsePayload,
            ResponseHeader,
        >,
        StrictNotSent<
            StrictRequestMut<
                Service,
                RequestPayload,
                RequestHeader,
                ResponsePayload,
                ResponseHeader,
            >,
        >,
    > {
        let parts = match self.parts {
            Some(parts) => parts,
            None => {
                fatal_panic!(from self, "This should never happen! A strict request was sent twice.")
            }
        };
        let outcome = {
            let shared = self.shared.lock();
            Self::send_locked(&shared, parts)
        };
        match outcome {
            Ok((pin, issue)) => {
                // 发布后的不可失败提交：parts 按值转给 Pending，本对象的 Drop 不再归还。
                self.parts = None;
                let this = core::mem::ManuallyDrop::new(self);
                let ptr = unsafe { core::ptr::read(&this.ptr) };
                let shared = unsafe { core::ptr::read(&this.shared) };
                Ok(StrictPendingResponse {
                    detached: Some(StrictDetached {
                        offset: parts.offset,
                        channel_id: parts.channel_id,
                        request_id: parts.request_id,
                        pin,
                    }),
                    ptr,
                    shared,
                    issue,
                    _response_payload: PhantomData,
                    _response_header: PhantomData,
                })
            }
            Err(reason) => Err(StrictNotSent {
                request: self,
                reason,
            }),
        }
    }

    /// 撤销本次预留：槽回 Free，按值消费本次原 pin 原件。
    fn rollback_reservation(shared: &ClientSharedState<Service>, slot_index: usize, pin: RoutePin) {
        shared.strict_slots()[slot_index] = StrictSlot::Free;
        shared.response_receiver.unpin_connection(pin);
    }

    fn send_locked(
        shared: &ClientSharedState<Service>,
        parts: LoanParts,
    ) -> Result<(RoutePin, Option<EnqueueIssue>), StrictSendRefusal> {
        shared.maintain_detached();

        if let Err(e) = shared.update_connections() {
            return Err(StrictSendRefusal::UpdateConnections(e));
        }

        let max_active = shared
            .request_sender
            .service_state
            .static_config()
            .request_response()
            .max_active_requests_per_client();
        if shared.active_request_counter.load(Ordering::Relaxed) >= max_active {
            return Err(StrictSendRefusal::ExceedsMaxActiveRequests);
        }

        // 注册表中恰一个 server，且为 Static。
        let mut servers = 0usize;
        let mut found: Option<(usize, u128, DataSegmentType)> = None;
        unsafe { &*shared.server_list_state.get() }.for_each(|index, details| {
            servers += 1;
            found = Some((index, details.server_id.value(), details.data_segment_type));
            CallbackProgression::Continue
        });
        let (index, server_id, segment_type) = match (servers, found) {
            (0, _) | (_, None) => return Err(StrictSendRefusal::NoServer),
            (1, Some(found)) => found,
            (count, _) => return Err(StrictSendRefusal::MultipleServers { count }),
        };
        if segment_type != DataSegmentType::Static {
            return Err(StrictSendRefusal::ServerNotStatic);
        }

        // 发送连接必须精确指向该 server（完整 id）。
        let sender_index = match shared.request_sender.resolve_connection(index, server_id) {
            Some(sender_index) => sender_index,
            None => {
                shared.retry_update.store(true, Ordering::Relaxed);
                return Err(StrictSendRefusal::TargetMismatch);
            }
        };

        // enqueue 之前：本 channel 固定槽须 Free，并真实 pin 该 index 上完整 id 一致的响应连接；
        // 只有真实 pin 成功才存在原 pin 原件。
        let slot_index = parts.channel_id.value();
        if !matches!(shared.strict_slots()[slot_index], StrictSlot::Free) {
            return Err(StrictSendRefusal::SlotBusy);
        }
        let pin = match shared.response_receiver.pin_connection(
            index,
            server_id,
            parts.channel_id,
            parts.request_id,
        ) {
            Some(pin) => pin,
            None => {
                shared.retry_update.store(true, Ordering::Relaxed);
                return Err(StrictSendRefusal::TargetMismatch);
            }
        };
        shared.strict_slots()[slot_index] = StrictSlot::Reserved;

        // CAL strict prepare：仅 CLOSED → 本请求。
        if let Err(e) = shared.response_receiver.strict_prepare_pinned(&pin) {
            Self::rollback_reservation(shared, slot_index, pin);
            return Err(StrictSendRefusal::Prepare(e));
        }

        // 唯一 enqueue（所有请求经 channel 0 的请求缓冲）。
        let receipt = match shared.request_sender.deliver_one_observed(
            sender_index,
            server_id,
            parts.offset,
            parts.sample_size,
            REQUEST_CHANNEL_ID,
        ) {
            Ok(receipt) => receipt,
            Err(_) => {
                shared.response_receiver.close_channel_pinned(&pin);
                Self::rollback_reservation(shared, slot_index, pin);
                shared.retry_update.store(true, Ordering::Relaxed);
                return Err(StrictSendRefusal::TargetMismatch);
            }
        };
        match receipt {
            TrySendReceipt::NotEnqueued(error) => {
                // 无远端接纳：撤销本次 prepare 与预留，原 request 完整返回。
                shared.response_receiver.close_channel_pinned(&pin);
                Self::rollback_reservation(shared, slot_index, pin);
                Err(StrictSendRefusal::NotEnqueued(error))
            }
            TrySendReceipt::Enqueued { issue, .. } => {
                // 发布后的不可失败提交：槽转 Live，loan→active 转账一次
                // （sample 的远端引用已在 deliver_one_observed 登记）；原 pin 原件按值交给 Pending。
                shared.strict_slots()[slot_index] = StrictSlot::Live;
                shared
                    .active_request_counter
                    .fetch_add(1, Ordering::Relaxed);
                shared
                    .request_sender
                    .loan_counter
                    .fetch_sub(1, Ordering::Relaxed);
                Ok((pin, issue))
            }
        }
    }
}

/// 发送在 publication 之前被拒绝：原 request 完整返回，可重发或丢弃（Drop 归原 loan）。
pub struct StrictNotSent<R> {
    /// 完整返回的原 request（loan / channel 不变）。
    pub request: R,
    /// 拒绝原因（publication 之前）。
    pub reason: StrictSendRefusal,
}

impl<R> Debug for StrictNotSent<R> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "StrictNotSent {{ reason: {:?} }}", self.reason)
    }
}

/// 已入队请求的 Pending：按值持有本地原件与原 pin 原件，私有持有真实回执的 issue；
/// Drop 时原件按值转入固定 Detached 槽并只发原 hint。
pub struct StrictPendingResponse<
    Service: service::Service,
    RequestPayload: Debug + ZeroCopySend + ?Sized,
    RequestHeader: Debug + ZeroCopySend,
    ResponsePayload: Debug + ZeroCopySend + ?Sized,
    ResponseHeader: Debug + ZeroCopySend,
> {
    detached: Option<StrictDetached>,
    ptr: RawSampleMut<RequestHeaderInternal, RequestHeader, RequestPayload>,
    shared: Service::ArcThreadSafetyPolicy<ClientSharedState<Service>>,
    issue: Option<EnqueueIssue>,
    _response_payload: PhantomData<ResponsePayload>,
    _response_header: PhantomData<ResponseHeader>,
}

impl<
    Service: service::Service,
    RequestPayload: Debug + ZeroCopySend + ?Sized,
    RequestHeader: Debug + ZeroCopySend,
    ResponsePayload: Debug + ZeroCopySend + ?Sized,
    ResponseHeader: Debug + ZeroCopySend,
> Debug
    for StrictPendingResponse<
        Service,
        RequestPayload,
        RequestHeader,
        ResponsePayload,
        ResponseHeader,
    >
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "StrictPendingResponse {{ detached: {:?}, issue: {:?} }}",
            self.detached, self.issue
        )
    }
}

impl<
    Service: service::Service,
    RequestPayload: Debug + ZeroCopySend + ?Sized,
    RequestHeader: Debug + ZeroCopySend,
    ResponsePayload: Debug + ZeroCopySend + ?Sized,
    ResponseHeader: Debug + ZeroCopySend,
> Drop
    for StrictPendingResponse<
        Service,
        RequestPayload,
        RequestHeader,
        ResponsePayload,
        ResponseHeader,
    >
{
    fn drop(&mut self) {
        // 按值把无 Arc 的原件（含原 pin 原件）转入本请求的 Live 槽；只发原 hint，不 client 自写
        // CLOSED，不分配、不失败。active 计数与原 pin 由槽继续持有，直到正常维护或最后 shared 退出。
        let Some(detached) = self.detached.take() else {
            return;
        };
        let shared = self.shared.lock();
        shared
            .response_receiver
            .set_disconnect_hint_pinned(&detached.pin);
        let slot = &mut shared.strict_slots()[detached.channel_id.value()];
        if !matches!(slot, StrictSlot::Live) {
            fatal_panic!(from self, "This should never happen! The strict slot of a live request is not marked live.");
        }
        *slot = StrictSlot::Detached(detached);
    }
}

impl<
    Service: service::Service,
    RequestPayload: Debug + ZeroCopySend + ?Sized,
    RequestHeader: Debug + ZeroCopySend,
    ResponsePayload: Debug + ZeroCopySend + ?Sized,
    ResponseHeader: Debug + ZeroCopySend,
> StrictPendingResponse<Service, RequestPayload, RequestHeader, ResponsePayload, ResponseHeader>
{
    fn parts(&self) -> &StrictDetached {
        self.detached
            .as_ref()
            .expect("pending holds its parts until Drop")
    }

    /// 仅测试：本 Pending 持有的原 pin 原件（只读借用）。
    #[cfg(test)]
    pub(crate) fn pin_for_test(&self) -> &RoutePin {
        &self.parts().pin
    }

    /// 仅测试：本 Pending 的本地 sample 引用（PointerOffset）。
    #[cfg(test)]
    pub(crate) fn offset_for_test(&self) -> PointerOffset {
        self.parts().offset
    }

    /// 真实入队回执附带的问题（post-push 旧件回收失败等）；`None` 表示干净入队。
    pub fn delivery_issue(&self) -> Option<&EnqueueIssue> {
        self.issue.as_ref()
    }

    /// 内部请求 header（node / client id / channel / 请求序号 / 元素数）。
    pub fn header(&self) -> &RequestHeaderInternal {
        self.ptr.as_header_ref()
    }

    /// 用户 header。
    pub fn user_header(&self) -> &RequestHeader {
        self.ptr.as_user_header_ref()
    }

    /// 载荷。
    pub fn payload(&self) -> &RequestPayload {
        self.ptr.as_payload_ref()
    }

    /// 本请求的 strict 序号。
    pub fn request_id(&self) -> RequestId {
        self.parts().request_id
    }

    /// 本请求占用的 response channel。
    pub fn channel_id(&self) -> ChannelId {
        self.parts().channel_id
    }

    /// 严格三态（Acquire），按原 pin 原件的 key / channel / 序号：持本次成功 prepare、原 pin，
    /// channel 未归还，故 `Closed` 即原请求正常关闭。
    pub fn connection_state(&self) -> StrictChannelStatus {
        self.shared
            .lock()
            .response_receiver
            .strict_status_pinned(&self.parts().pin)
    }

    /// 只发原 hint（同源 set_hint：只匹配原请求，不能从终止值转换）。
    pub fn set_disconnect_hint(&self) {
        self.shared
            .lock()
            .response_receiver
            .set_disconnect_hint_pinned(&self.parts().pin);
    }

    /// 原连接、原 channel 上是否已有待收回复（只读观测，不维护、不 update）。
    pub fn has_response(&self) -> bool {
        self.shared
            .lock()
            .response_receiver
            .has_data_pinned(&self.parts().pin)
    }

    /// 同一 shared 锁内：先推进其他 Detached 的有界维护，再尝试 update 并记住其 Err，然后按原
    /// pin 原件真收；Some 交分类器，收包 Err 保 Pending，None 才返回原 update Err / None。
    /// 非本请求代次的回复立即归还并继续。
    fn receive_raw(
        &self,
    ) -> Result<
        Option<(
            crate::port::details::chunk_details::ChunkDetails,
            crate::port::details::chunk::Chunk,
        )>,
        ReceiveError,
    > {
        let parts = self.parts();
        let shared = self.shared.lock();
        shared.maintain_detached();
        let update_error = shared.update_connections().err();
        loop {
            match shared.response_receiver.receive_pinned(&parts.pin) {
                Ok(Some((details, chunk))) => {
                    let header = unsafe { &*(chunk.header as *const ResponseHeaderInternal) };
                    if header.request_id != parts.request_id {
                        shared
                            .response_receiver
                            .release_offset(&details, parts.channel_id);
                        continue;
                    }
                    return Ok(Some((details, chunk)));
                }
                Ok(None) => {
                    return match update_error {
                        Some(e) => Err(ReceiveError::ConnectionFailure(e)),
                        None => Ok(None),
                    };
                }
                Err(e) => return Err(e),
            }
        }
    }
}

impl<
    Service: service::Service,
    RequestPayload: Debug + ZeroCopySend + ?Sized,
    RequestHeader: Debug + ZeroCopySend,
    ResponsePayload: Debug + ZeroCopySend + Sized,
    ResponseHeader: Debug + ZeroCopySend,
> StrictPendingResponse<Service, RequestPayload, RequestHeader, ResponsePayload, ResponseHeader>
{
    /// 接收唯一 server 的回复（typed 载荷）。返回的 `Response` 持有 shared 引用，Drop 时归还原 borrow。
    pub fn receive(
        &self,
    ) -> Result<Option<Response<Service, ResponsePayload, ResponseHeader>>, ReceiveError> {
        match self.receive_raw()? {
            None => Ok(None),
            Some((details, chunk)) => Ok(Some(Response {
                details,
                client_shared_state: self.shared.clone(),
                channel_id: self.parts().channel_id,
                ptr: unsafe {
                    crate::raw_sample::RawSample::new_unchecked(
                        chunk.header.cast(),
                        chunk.user_header.cast(),
                        chunk.payload.cast::<ResponsePayload>(),
                    )
                },
            })),
        }
    }
}

impl<
    Service: service::Service,
    RequestPayload: Debug + ZeroCopySend + ?Sized,
    RequestHeader: Debug + ZeroCopySend,
    ResponsePayload: Debug + ZeroCopySend,
    ResponseHeader: Debug + ZeroCopySend,
> StrictPendingResponse<Service, RequestPayload, RequestHeader, [ResponsePayload], ResponseHeader>
{
    /// 接收唯一 server 的回复（切片载荷）。
    pub fn receive(
        &self,
    ) -> Result<Option<Response<Service, [ResponsePayload], ResponseHeader>>, ReceiveError> {
        match self.receive_raw()? {
            None => Ok(None),
            Some((details, chunk)) => {
                let header = unsafe { &*(chunk.header as *const ResponseHeaderInternal) };
                let number_of_elements = header.number_of_elements as usize;
                Ok(Some(Response {
                    details,
                    client_shared_state: self.shared.clone(),
                    channel_id: self.parts().channel_id,
                    ptr: unsafe {
                        crate::raw_sample::RawSample::new_slice_unchecked(
                            chunk.header.cast(),
                            chunk.user_header.cast(),
                            core::ptr::slice_from_raw_parts(
                                chunk.payload.cast::<ResponsePayload>(),
                                number_of_elements,
                            ),
                        )
                    },
                }))
            }
        }
    }
}
