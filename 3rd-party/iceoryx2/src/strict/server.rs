// RR R2（robot-system 本地补丁）：受检的严格唯一回复 server。
//
// 许可：与所在 crate 相同（MIT OR Apache-2.0）。

use core::{fmt::Debug, marker::PhantomData, mem::MaybeUninit};

use alloc::format;
use iceoryx2_bb_elementary_traits::zero_copy_send::ZeroCopySend;
use iceoryx2_bb_posix::unique_system_id::UniqueSystemId;
use iceoryx2_cal::arc_sync_policy::ArcSyncPolicy;
use iceoryx2_cal::shm_allocator::{AllocationStrategy, PointerOffset};
use iceoryx2_cal::zero_copy_connection::{
    ChannelId, EnqueueIssue, StrictChannelStatus, TrySendReceipt,
};
use iceoryx2_log::fail;

use crate::active_request::RequestId;
use crate::identifiers::{UniqueClientId, UniqueServerId};
use crate::port::LoanError;
use crate::port::details::chunk_details::ChunkDetails;
use crate::port::details::sender::ExactTargetError;
use crate::port::server::{Server, SharedServerState};
use crate::port::update_connections::ConnectionFailure;
use crate::prelude::{BackpressureStrategy, PortFactory};
use crate::raw_sample::{RawSample, RawSampleMut};
use crate::service;
use crate::service::header::request_response::RequestHeader as RequestHeaderInternal;
use crate::service::header::request_response::ResponseHeader as ResponseHeaderInternal;
use crate::service::port_factory::server::PortFactoryServer;

use super::{StrictCreateError, StrictReceiveError, StrictResponseSendError};

/// 所有请求经 channel 0 的请求缓冲接收。
const REQUEST_CHANNEL_ID: ChannelId = ChannelId::new(0);

/// 严格唯一回复 server：只提供受检的 receive 入口；每个 Active 按值进入唯一 response loan。
#[derive(Debug)]
pub struct StrictServer<
    Service: service::Service,
    RequestPayload: Debug + ZeroCopySend + ?Sized,
    RequestHeader: Debug + ZeroCopySend,
    ResponsePayload: Debug + ZeroCopySend + ?Sized,
    ResponseHeader: Debug + ZeroCopySend,
> {
    server: Server<Service, RequestPayload, RequestHeader, ResponsePayload, ResponseHeader>,
}

impl<
    Service: service::Service,
    RequestPayload: Debug + ZeroCopySend + ?Sized,
    RequestHeader: Debug + ZeroCopySend,
    ResponsePayload: Debug + ZeroCopySend + ?Sized,
    ResponseHeader: Debug + ZeroCopySend,
> StrictServer<Service, RequestPayload, RequestHeader, ResponsePayload, ResponseHeader>
{
    pub(crate) fn new(
        factory: PortFactoryServer<
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
                "Unable to create strict server since the service allows {} servers instead of exactly one.",
                static_config.max_servers());
        }
        if factory.config.allocation_strategy != AllocationStrategy::Static {
            fail!(from origin, with StrictCreateError::AllocationNotStatic,
                "Unable to create strict server since the allocation strategy is not Static.");
        }
        if factory.config.backpressure_strategy != BackpressureStrategy::DiscardData {
            fail!(from origin, with StrictCreateError::BackpressureNotDiscardData,
                "Unable to create strict server since the backpressure strategy is not DiscardData.");
        }
        if factory.backpressure_handler.is_some() {
            fail!(from origin, with StrictCreateError::CustomBackpressureHandler,
                "Unable to create strict server since a custom backpressure handler is set.");
        }
        if static_config.has_safe_overflow_for_requests() {
            fail!(from origin, with StrictCreateError::RequestOverflowEnabled,
                "Unable to create strict server since the service enables safe overflow for requests.");
        }

        // 容量 / 布局 / 固定表均在 `Server::new` 内受检计算与可失败预留，typed 经
        // `ServerCreateError::{CapacityOverflow, InsufficientMemory}` 返回。
        let server = match Server::new(factory) {
            Ok(server) => server,
            Err(e) => {
                fail!(from origin, with StrictCreateError::Server(e),
                    "Unable to create strict server since the underlying server could not be created ({e:?}).");
            }
        };
        Ok(Self { server })
    }

    /// 完整原 server id。
    pub fn id(&self) -> UniqueServerId {
        self.server.id()
    }

    /// 仅测试：共享状态句柄。
    #[cfg(test)]
    pub(crate) fn shared_for_test(
        &self,
    ) -> &Service::ArcThreadSafetyPolicy<SharedServerState<Service>> {
        &self.server.shared_state
    }

    /// 是否有待接收的请求（先 update 连接）。
    pub fn has_requests(&self) -> Result<bool, ConnectionFailure> {
        self.server.has_requests()
    }

    /// 共用接收内核：真核接收连接身份 == header client id，并精确定位该 client 的响应连接；
    /// 无响应连接（client 已退出）的请求归还并跳过——它无法被严格回复。
    fn receive_verified(
        &self,
    ) -> Result<Option<(ChunkDetails, crate::port::details::chunk::Chunk, usize)>, StrictReceiveError>
    {
        loop {
            let (details, chunk) = match self.server.receive_impl() {
                Ok(Some(received)) => received,
                Ok(None) => return Ok(None),
                Err(e) => return Err(StrictReceiveError::Receive(e)),
            };
            let header = unsafe { &*(chunk.header as *const RequestHeaderInternal) };
            let header_client = header.client_id.value();
            if details.origin != header_client {
                let origin = details.origin;
                self.server
                    .shared_state
                    .lock()
                    .request_receiver
                    .release_offset(&details, REQUEST_CHANNEL_ID);
                return Err(StrictReceiveError::OriginMismatch {
                    origin,
                    header_client,
                });
            }
            let shared = self.server.shared_state.lock();
            match shared.response_sender.get_connection_id_of(details.origin) {
                Some(index) => {
                    drop(shared);
                    return Ok(Some((details, chunk, index)));
                }
                None => {
                    shared
                        .request_receiver
                        .release_offset(&details, REQUEST_CHANNEL_ID);
                    continue;
                }
            }
        }
    }
}

impl<
    Service: service::Service,
    RequestPayload: Debug + ZeroCopySend,
    RequestHeader: Debug + ZeroCopySend,
    ResponsePayload: Debug + ZeroCopySend + ?Sized,
    ResponseHeader: Debug + ZeroCopySend,
> StrictServer<Service, RequestPayload, RequestHeader, ResponsePayload, ResponseHeader>
{
    /// 接收一个受检请求（typed 载荷）。
    #[allow(clippy::type_complexity)]
    pub fn receive(
        &self,
    ) -> Result<
        Option<
            StrictActiveRequest<
                Service,
                RequestPayload,
                RequestHeader,
                ResponsePayload,
                ResponseHeader,
            >,
        >,
        StrictReceiveError,
    > {
        let Some((details, chunk, index)) = self.receive_verified()? else {
            return Ok(None);
        };
        let header = unsafe { &*(chunk.header as *const RequestHeaderInternal) };
        Ok(Some(StrictActiveRequest {
            core: StrictActiveCore {
                ptr: unsafe {
                    RawSample::new_unchecked(
                        chunk.header.cast(),
                        chunk.user_header.cast(),
                        chunk.payload.cast::<RequestPayload>(),
                    )
                },
                client_id: header.client_id,
                request_id: header.request_id,
                channel_id: header.channel_id,
                connection_hint: index,
                details: Some(details),
                shared: self.server.shared_state.clone(),
            },
            _response_payload: PhantomData,
            _response_header: PhantomData,
        }))
    }
}

impl<
    Service: service::Service,
    RequestPayload: Debug + ZeroCopySend,
    RequestHeader: Debug + ZeroCopySend,
    ResponsePayload: Debug + ZeroCopySend + ?Sized,
    ResponseHeader: Debug + ZeroCopySend,
> StrictServer<Service, [RequestPayload], RequestHeader, ResponsePayload, ResponseHeader>
{
    /// 接收一个受检请求（切片载荷）。
    #[allow(clippy::type_complexity)]
    pub fn receive(
        &self,
    ) -> Result<
        Option<
            StrictActiveRequest<
                Service,
                [RequestPayload],
                RequestHeader,
                ResponsePayload,
                ResponseHeader,
            >,
        >,
        StrictReceiveError,
    > {
        let Some((details, chunk, index)) = self.receive_verified()? else {
            return Ok(None);
        };
        let header = unsafe { &*(chunk.header as *const RequestHeaderInternal) };
        Ok(Some(StrictActiveRequest {
            core: StrictActiveCore {
                ptr: unsafe {
                    RawSample::new_slice_unchecked(
                        chunk.header.cast(),
                        chunk.user_header.cast(),
                        core::ptr::slice_from_raw_parts(
                            chunk.payload.cast::<RequestPayload>(),
                            header.number_of_elements() as _,
                        ),
                    )
                },
                client_id: header.client_id,
                request_id: header.request_id,
                channel_id: header.channel_id,
                connection_hint: index,
                details: Some(details),
                shared: self.server.shared_state.clone(),
            },
            _response_payload: PhantomData,
            _response_header: PhantomData,
        }))
    }
}

/// 受检活动请求的内核（与响应载荷类型无关）：保存完整原 client id 与原请求 chunk；
/// Drop 即 finish（归还请求 chunk + 精确 id 的同源关闭），只执行一次。
pub struct StrictActiveCore<
    Service: service::Service,
    RequestPayload: Debug + ZeroCopySend + ?Sized,
    RequestHeader: Debug + ZeroCopySend,
> {
    ptr: RawSample<RequestHeaderInternal, RequestHeader, RequestPayload>,
    client_id: UniqueClientId,
    request_id: RequestId,
    channel_id: ChannelId,
    /// 原响应连接 index，仅作提示；真实目标以 `client_id` 精确定位。
    connection_hint: usize,
    details: Option<ChunkDetails>,
    shared: Service::ArcThreadSafetyPolicy<SharedServerState<Service>>,
}

impl<
    Service: service::Service,
    RequestPayload: Debug + ZeroCopySend + ?Sized,
    RequestHeader: Debug + ZeroCopySend,
> Debug for StrictActiveCore<Service, RequestPayload, RequestHeader>
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "StrictActiveCore {{ client_id: {:?}, request_id: {}, channel_id: {} }}",
            self.client_id,
            self.request_id.value(),
            self.channel_id.value()
        )
    }
}

impl<
    Service: service::Service,
    RequestPayload: Debug + ZeroCopySend + ?Sized,
    RequestHeader: Debug + ZeroCopySend,
> Drop for StrictActiveCore<Service, RequestPayload, RequestHeader>
{
    fn drop(&mut self) {
        self.finish();
    }
}

impl<
    Service: service::Service,
    RequestPayload: Debug + ZeroCopySend + ?Sized,
    RequestHeader: Debug + ZeroCopySend,
> StrictActiveCore<Service, RequestPayload, RequestHeader>
{
    /// 仅故障测试：真实接收原件的 offset。
    #[cfg(all(test, feature = "r2-test-fixtures"))]
    pub(crate) fn offset_for_test(&self) -> PointerOffset {
        self.details
            .as_ref()
            .expect("尚未 finish 的原 Active")
            .offset
    }

    /// finish 原 Active：归还请求 chunk 给原接收连接，并按完整原 client id 精确关闭该请求的
    /// 响应 channel（同源 finish）。原 client 已不在则不关闭任何新代。只执行一次。
    fn finish(&mut self) {
        let Some(details) = self.details.take() else {
            return;
        };
        let shared = self.shared.lock();
        shared
            .request_receiver
            .release_offset(&details, REQUEST_CHANNEL_ID);
        if let Some(index) = shared
            .response_sender
            .resolve_connection(self.connection_hint, self.client_id.value())
        {
            shared
                .response_sender
                .close_channel(self.channel_id, index, self.request_id);
        }
    }

    /// 载荷。
    pub fn payload(&self) -> &RequestPayload {
        self.ptr.as_payload_ref()
    }

    /// 用户 header。
    pub fn user_header(&self) -> &RequestHeader {
        self.ptr.as_user_header_ref()
    }

    /// 内部请求 header（node / client id / channel / 请求序号 / 元素数）。
    pub fn header(&self) -> &RequestHeaderInternal {
        self.ptr.as_header_ref()
    }

    /// 完整原 client id（来自真实接收连接，已与 header 核对一致）。
    pub fn origin(&self) -> UniqueClientId {
        self.client_id
    }

    /// 本请求的 strict 序号。
    pub fn request_id(&self) -> RequestId {
        self.request_id
    }

    /// 本请求占用的 response channel。
    pub fn channel_id(&self) -> ChannelId {
        self.channel_id
    }

    /// 严格三态（精确 id；原 client 缺失即 Unknown）。
    pub fn connection_state(&self) -> StrictChannelStatus {
        self.shared
            .lock()
            .response_sender
            .strict_channel_status_by_id(
                self.channel_id,
                self.connection_hint,
                self.client_id.value(),
                self.request_id,
            )
    }

    /// 原 client 是否已对本请求发出断开提示（精确 id；原 client 缺失即 `false`）。
    pub fn has_disconnect_hint(&self) -> bool {
        let shared = self.shared.lock();
        match shared
            .response_sender
            .resolve_connection(self.connection_hint, self.client_id.value())
        {
            Some(index) => {
                shared
                    .response_sender
                    .has_disconnect_hint(self.channel_id, index, self.request_id)
            }
            None => false,
        }
    }

    /// 共用 loan 内核：分配响应 chunk 并写内部 header。
    fn loan_parts(&self, number_of_elements: usize) -> Result<LoanedResponse, LoanError> {
        let shared = self.shared.lock();
        let layout = shared.response_sender.sample_layout(number_of_elements);
        let chunk = shared.response_sender.allocate(layout)?;
        let header_ptr: *mut ResponseHeaderInternal = chunk.header.cast();
        unsafe {
            header_ptr.write(ResponseHeaderInternal {
                node_id: *shared.response_sender.shared_node.id(),
                server_id: UniqueServerId(UniqueSystemId::from(
                    shared.response_sender.sender_port_id,
                )),
                request_id: self.request_id,
                number_of_elements: number_of_elements as _,
            });
        }
        Ok(LoanedResponse {
            offset: chunk.offset,
            sample_size: chunk.size,
            header: chunk.header,
            user_header: chunk.user_header,
            payload: chunk.payload,
        })
    }
}

/// 受检的活动请求：按值进入唯一 response loan，不可 Clone、无 Deref、不暴露普通 Active。
/// Drop（未回复）即 finish 原 Active。
pub struct StrictActiveRequest<
    Service: service::Service,
    RequestPayload: Debug + ZeroCopySend + ?Sized,
    RequestHeader: Debug + ZeroCopySend,
    ResponsePayload: Debug + ZeroCopySend + ?Sized,
    ResponseHeader: Debug + ZeroCopySend,
> {
    core: StrictActiveCore<Service, RequestPayload, RequestHeader>,
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
    for StrictActiveRequest<Service, RequestPayload, RequestHeader, ResponsePayload, ResponseHeader>
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "StrictActiveRequest {{ core: {:?} }}", self.core)
    }
}

impl<
    Service: service::Service,
    RequestPayload: Debug + ZeroCopySend + ?Sized,
    RequestHeader: Debug + ZeroCopySend,
    ResponsePayload: Debug + ZeroCopySend + ?Sized,
    ResponseHeader: Debug + ZeroCopySend,
> StrictActiveRequest<Service, RequestPayload, RequestHeader, ResponsePayload, ResponseHeader>
{
    /// 载荷。
    pub fn payload(&self) -> &RequestPayload {
        self.core.payload()
    }

    /// 用户 header。
    pub fn user_header(&self) -> &RequestHeader {
        self.core.user_header()
    }

    /// 内部请求 header（node / client id / channel / 请求序号 / 元素数）。
    pub fn header(&self) -> &RequestHeaderInternal {
        self.core.header()
    }

    /// 完整原 client id（来自真实接收连接，已与 header 核对一致）。
    pub fn origin(&self) -> UniqueClientId {
        self.core.origin()
    }

    /// 本请求的 strict 序号。
    pub fn request_id(&self) -> RequestId {
        self.core.request_id()
    }

    /// 本请求占用的 response channel。
    pub fn channel_id(&self) -> ChannelId {
        self.core.channel_id()
    }

    /// 严格三态（精确 id；原 client 缺失即 `Unknown`）。
    pub fn connection_state(&self) -> StrictChannelStatus {
        self.core.connection_state()
    }

    /// 原 client 是否已对本请求发出断开提示（精确 id；原 client 缺失即 `false`）。
    pub fn has_disconnect_hint(&self) -> bool {
        self.core.has_disconnect_hint()
    }
}

/// loan 失败：Active 原样返回，不丢原件。
pub struct StrictLoanRefused<A> {
    /// 原样返回的 Active（未丢失）。
    pub active: A,
    /// 底层 loan 失败原因。
    pub error: LoanError,
}

impl<A> Debug for StrictLoanRefused<A> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "StrictLoanRefused {{ error: {:?} }}", self.error)
    }
}

struct LoanedResponse {
    offset: PointerOffset,
    sample_size: usize,
    header: *mut u8,
    user_header: *mut u8,
    payload: *mut u8,
}

impl<
    Service: service::Service,
    RequestPayload: Debug + ZeroCopySend + ?Sized,
    RequestHeader: Debug + ZeroCopySend,
    ResponsePayload: Debug + ZeroCopySend + Sized,
    ResponseHeader: Default + Debug + ZeroCopySend,
> StrictActiveRequest<Service, RequestPayload, RequestHeader, ResponsePayload, ResponseHeader>
{
    /// 按值进入唯一 response loan（typed 载荷）。
    #[allow(clippy::type_complexity)]
    pub fn loan_uninit(
        self,
    ) -> Result<
        StrictResponseMutUninit<
            Service,
            RequestPayload,
            RequestHeader,
            MaybeUninit<ResponsePayload>,
            ResponseHeader,
        >,
        StrictLoanRefused<Self>,
    > {
        let loaned = match self.core.loan_parts(1) {
            Ok(loaned) => loaned,
            Err(error) => {
                return Err(StrictLoanRefused {
                    active: self,
                    error,
                });
            }
        };
        let header_ptr: *mut ResponseHeaderInternal = loaned.header.cast();
        let user_header_ptr: *mut ResponseHeader = loaned.user_header.cast();
        let ptr = unsafe {
            RawSampleMut::<ResponseHeaderInternal, ResponseHeader, MaybeUninit<ResponsePayload>>::new_unchecked(
                header_ptr,
                user_header_ptr,
                loaned.payload.cast(),
            )
        };
        let Self { core, .. } = self;
        // 先建立 loan RAII（含原 Active 内核：unwind 时先释放本地 loan，再 finish 原 Active），
        // 再执行可能 panic 的用户 header 初始化。
        let response = StrictResponseMut {
            ptr,
            offset: loaned.offset,
            sample_size: loaned.sample_size,
            loan_released: false,
            active: core,
            _response_payload: PhantomData,
        };
        unsafe { user_header_ptr.write(ResponseHeader::default()) };
        Ok(StrictResponseMutUninit { response })
    }
}

impl<
    Service: service::Service,
    RequestPayload: Debug + ZeroCopySend + ?Sized,
    RequestHeader: Debug + ZeroCopySend,
    ResponsePayload: Debug + ZeroCopySend + 'static,
    ResponseHeader: Default + Debug + ZeroCopySend,
> StrictActiveRequest<Service, RequestPayload, RequestHeader, [ResponsePayload], ResponseHeader>
{
    /// 按值进入唯一 response loan（切片载荷）；Static 分配下 `slice_len` 不得超过 `initial_max_slice_len`。
    #[allow(clippy::type_complexity)]
    pub fn loan_slice_uninit(
        self,
        slice_len: usize,
    ) -> Result<
        StrictResponseMutUninit<
            Service,
            RequestPayload,
            RequestHeader,
            [MaybeUninit<ResponsePayload>],
            ResponseHeader,
        >,
        StrictLoanRefused<Self>,
    > {
        let max_slice_len = self.core.shared.lock().config.initial_max_slice_len;
        if max_slice_len < slice_len {
            return Err(StrictLoanRefused {
                active: self,
                error: LoanError::ExceedsMaxLoanSize,
            });
        }
        let loaned = match self.core.loan_parts(slice_len) {
            Ok(loaned) => loaned,
            Err(error) => {
                return Err(StrictLoanRefused {
                    active: self,
                    error,
                });
            }
        };
        let header_ptr: *mut ResponseHeaderInternal = loaned.header.cast();
        let user_header_ptr: *mut ResponseHeader = loaned.user_header.cast();
        let ptr = unsafe {
            RawSampleMut::<ResponseHeaderInternal, ResponseHeader, [MaybeUninit<ResponsePayload>]>::new_unchecked(
                header_ptr,
                user_header_ptr,
                core::ptr::slice_from_raw_parts_mut(loaned.payload.cast(), slice_len),
            )
        };
        let Self { core, .. } = self;
        // 先建立 loan RAII，再执行可能 panic 的用户 header 初始化。
        let response = StrictResponseMut {
            ptr,
            offset: loaned.offset,
            sample_size: loaned.sample_size,
            loan_released: false,
            active: core,
            _response_payload: PhantomData,
        };
        unsafe { user_header_ptr.write(ResponseHeader::default()) };
        Ok(StrictResponseMutUninit { response })
    }
}

/// 未初始化的唯一回复；持有原 Active 内核。Drop（未发送）先释放本地 loan，再 finish 原 Active。
pub struct StrictResponseMutUninit<
    Service: service::Service,
    RequestPayload: Debug + ZeroCopySend + ?Sized,
    RequestHeader: Debug + ZeroCopySend,
    ResponsePayload: Debug + ZeroCopySend + ?Sized,
    ResponseHeader: Debug + ZeroCopySend,
> {
    response:
        StrictResponseMut<Service, RequestPayload, RequestHeader, ResponsePayload, ResponseHeader>,
}

impl<
    Service: service::Service,
    RequestPayload: Debug + ZeroCopySend + ?Sized,
    RequestHeader: Debug + ZeroCopySend,
    ResponsePayload: Debug + ZeroCopySend + ?Sized,
    ResponseHeader: Debug + ZeroCopySend,
> Debug
    for StrictResponseMutUninit<
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
            "StrictResponseMutUninit {{ response: {:?} }}",
            self.response
        )
    }
}

impl<
    Service: service::Service,
    RequestPayload: Debug + ZeroCopySend + ?Sized,
    RequestHeader: Debug + ZeroCopySend,
    ResponsePayload: Debug + ZeroCopySend + ?Sized,
    ResponseHeader: Debug + ZeroCopySend,
> StrictResponseMutUninit<Service, RequestPayload, RequestHeader, ResponsePayload, ResponseHeader>
{
    /// 可变用户 header。
    pub fn user_header_mut(&mut self) -> &mut ResponseHeader {
        self.response.user_header_mut()
    }

    /// 可变载荷。
    pub fn payload_mut(&mut self) -> &mut ResponsePayload {
        self.response.payload_mut()
    }

    /// 原请求的只读内核视图（Active 仍由本对象持有）。
    pub fn request(&self) -> &StrictActiveCore<Service, RequestPayload, RequestHeader> {
        &self.response.active
    }
}

impl<
    Service: service::Service,
    RequestPayload: Debug + ZeroCopySend + ?Sized,
    RequestHeader: Debug + ZeroCopySend,
    ResponsePayload: Debug + ZeroCopySend + Sized,
    ResponseHeader: Debug + ZeroCopySend,
>
    StrictResponseMutUninit<
        Service,
        RequestPayload,
        RequestHeader,
        MaybeUninit<ResponsePayload>,
        ResponseHeader,
    >
{
    /// 写入载荷并标记为已初始化。
    pub fn write_payload(
        mut self,
        value: ResponsePayload,
    ) -> StrictResponseMut<Service, RequestPayload, RequestHeader, ResponsePayload, ResponseHeader>
    {
        self.payload_mut().write(value);
        unsafe { self.assume_init() }
    }

    /// # Safety
    ///
    /// 调用方须已初始化整个载荷。
    pub unsafe fn assume_init(
        self,
    ) -> StrictResponseMut<Service, RequestPayload, RequestHeader, ResponsePayload, ResponseHeader>
    {
        // 按字段转移原所有权（原 Active 内核、loan 原件），只把已初始化的 payload 指针转换为
        // 已初始化类型；不对外层泛型结构整体位拷贝。
        let StrictResponseMutUninit { response } = self;
        let response = core::mem::ManuallyDrop::new(response);
        let active = unsafe { core::ptr::read(&response.active) };
        let ptr = unsafe {
            RawSampleMut::new_unchecked(
                response.ptr.header_ptr(),
                response.ptr.user_header_ptr(),
                response.ptr.payload_ptr().cast::<ResponsePayload>(),
            )
        };
        StrictResponseMut {
            ptr,
            offset: response.offset,
            sample_size: response.sample_size,
            loan_released: response.loan_released,
            active,
            _response_payload: PhantomData,
        }
    }
}

impl<
    Service: service::Service,
    RequestPayload: Debug + ZeroCopySend + ?Sized,
    RequestHeader: Debug + ZeroCopySend,
    ResponsePayload: Debug + ZeroCopySend,
    ResponseHeader: Debug + ZeroCopySend,
>
    StrictResponseMutUninit<
        Service,
        RequestPayload,
        RequestHeader,
        [MaybeUninit<ResponsePayload>],
        ResponseHeader,
    >
{
    /// # Safety
    ///
    /// 调用方须已初始化整个切片。
    pub unsafe fn assume_init(
        self,
    ) -> StrictResponseMut<Service, RequestPayload, RequestHeader, [ResponsePayload], ResponseHeader>
    {
        let StrictResponseMutUninit { response } = self;
        let response = core::mem::ManuallyDrop::new(response);
        let active = unsafe { core::ptr::read(&response.active) };
        let payload = response.ptr.payload_ptr();
        let len = payload.len();
        let ptr = unsafe {
            RawSampleMut::new_unchecked(
                response.ptr.header_ptr(),
                response.ptr.user_header_ptr(),
                core::ptr::slice_from_raw_parts_mut(
                    (payload as *mut MaybeUninit<ResponsePayload>).cast::<ResponsePayload>(),
                    len,
                ),
            )
        };
        StrictResponseMut {
            ptr,
            offset: response.offset,
            sample_size: response.sample_size,
            loan_released: response.loan_released,
            active,
            _response_payload: PhantomData,
        }
    }

    /// 用初始化函数逐元素写入并标记为已初始化。
    pub fn write_from_fn<F: FnMut(usize) -> ResponsePayload>(
        mut self,
        mut initializer: F,
    ) -> StrictResponseMut<Service, RequestPayload, RequestHeader, [ResponsePayload], ResponseHeader>
    {
        for (i, element) in self.payload_mut().iter_mut().enumerate() {
            element.write(initializer(i));
        }
        unsafe { self.assume_init() }
    }
}

impl<
    Service: service::Service,
    RequestPayload: Debug + ZeroCopySend + ?Sized,
    RequestHeader: Debug + ZeroCopySend,
    ResponsePayload: Debug + Copy + ZeroCopySend,
    ResponseHeader: Debug + ZeroCopySend,
>
    StrictResponseMutUninit<
        Service,
        RequestPayload,
        RequestHeader,
        [MaybeUninit<ResponsePayload>],
        ResponseHeader,
    >
{
    /// 从切片逐元素写入并标记为已初始化（以载荷切片长度为准）。
    pub fn write_from_slice(
        mut self,
        value: &[ResponsePayload],
    ) -> StrictResponseMut<Service, RequestPayload, RequestHeader, [ResponsePayload], ResponseHeader>
    {
        // 长度必须完全一致（沿 `copy_from_slice` 语义 panic）；panic 时先释放本地 loan，再 finish 原 Active。
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

/// 已初始化的唯一回复；持有原 Active 内核。发送或丢弃并释放本地 loan 之后，字段析构才 finish 原 Active。
pub struct StrictResponseMut<
    Service: service::Service,
    RequestPayload: Debug + ZeroCopySend + ?Sized,
    RequestHeader: Debug + ZeroCopySend,
    ResponsePayload: Debug + ZeroCopySend + ?Sized,
    ResponseHeader: Debug + ZeroCopySend,
> {
    ptr: RawSampleMut<ResponseHeaderInternal, ResponseHeader, ResponsePayload>,
    offset: PointerOffset,
    sample_size: usize,
    loan_released: bool,
    // 字段顺序：Active 内核在本地 loan 释放（Drop body）之后才析构，即 finish 晚于 loan 释放。
    active: StrictActiveCore<Service, RequestPayload, RequestHeader>,
    _response_payload: PhantomData<ResponsePayload>,
}

impl<
    Service: service::Service,
    RequestPayload: Debug + ZeroCopySend + ?Sized,
    RequestHeader: Debug + ZeroCopySend,
    ResponsePayload: Debug + ZeroCopySend + ?Sized,
    ResponseHeader: Debug + ZeroCopySend,
> Debug
    for StrictResponseMut<Service, RequestPayload, RequestHeader, ResponsePayload, ResponseHeader>
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "StrictResponseMut {{ offset: {:?}, sample_size: {}, active: {:?} }}",
            self.offset, self.sample_size, self.active
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
    for StrictResponseMut<Service, RequestPayload, RequestHeader, ResponsePayload, ResponseHeader>
{
    fn drop(&mut self) {
        // 无论是否发送成功，本地 loan 只释放一次；随后 `active` 字段析构执行 finish。
        if !self.loan_released {
            self.loan_released = true;
            self.active
                .shared
                .lock()
                .response_sender
                .return_loaned_sample(self.offset);
        }
    }
}

/// 回复已真实入队的回执；`issue` 记录 post-push 的旧件回收问题（新回复仍已投递）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StrictSent {
    /// post-push 的旧件回收问题；`None` 表示干净入队。
    pub issue: Option<EnqueueIssue>,
}

impl<
    Service: service::Service,
    RequestPayload: Debug + ZeroCopySend + ?Sized,
    RequestHeader: Debug + ZeroCopySend,
    ResponsePayload: Debug + ZeroCopySend + ?Sized,
    ResponseHeader: Debug + ZeroCopySend,
> StrictResponseMut<Service, RequestPayload, RequestHeader, ResponsePayload, ResponseHeader>
{
    /// 内部响应 header（node / server id / 请求序号 / 元素数）。
    pub fn header(&self) -> &ResponseHeaderInternal {
        self.ptr.as_header_ref()
    }

    /// 用户 header。
    pub fn user_header(&self) -> &ResponseHeader {
        self.ptr.as_user_header_ref()
    }

    /// 可变用户 header。
    pub fn user_header_mut(&mut self) -> &mut ResponseHeader {
        self.ptr.as_user_header_mut()
    }

    /// 载荷。
    pub fn payload(&self) -> &ResponsePayload {
        self.ptr.as_payload_ref()
    }

    /// 可变载荷。
    pub fn payload_mut(&mut self) -> &mut ResponsePayload {
        self.ptr.as_payload_mut()
    }

    /// 原请求的只读内核视图。
    pub fn request(&self) -> &StrictActiveCore<Service, RequestPayload, RequestHeader> {
        &self.active
    }

    /// 仅测试：本回复的本地 sample 引用（PointerOffset）。
    #[cfg(test)]
    pub(crate) fn offset_for_test(&self) -> PointerOffset {
        self.offset
    }

    /// 唯一回复：同 shared 锁内按完整原 client id 精确定位一次并 `try_send_observed` 一次。
    /// 无论结果如何，本地 loan 随后按原件释放一次，再 finish 原 Active；不 close 后再 enqueue。
    pub fn send(self) -> Result<StrictSent, StrictResponseSendError> {
        let outcome = {
            let shared = self.active.shared.lock();
            let client_id = self.active.client_id.value();
            let hint = self.active.connection_hint;
            if let Err(e) = shared.update_connections() {
                Err(StrictResponseSendError::UpdateConnections(e))
            } else {
                // update 后原 client 可能已在别的 index：同一锁内先按完整原 id 精确定位，再唯一投递。
                match shared.response_sender.resolve_connection(hint, client_id) {
                    None => Err(exact_target_send_error(
                        shared.response_sender.exact_target_error(hint, client_id),
                    )),
                    Some(index) => match shared.response_sender.deliver_one_observed(
                        index,
                        client_id,
                        self.offset,
                        self.sample_size,
                        self.active.channel_id,
                    ) {
                        Ok(TrySendReceipt::Enqueued { issue, .. }) => Ok(StrictSent { issue }),
                        Ok(TrySendReceipt::NotEnqueued(error)) => {
                            Err(StrictResponseSendError::NotEnqueued(error))
                        }
                        Err(e) => Err(exact_target_send_error(e)),
                    },
                }
            }
        };
        // `self` 在此析构：先释放本地 loan（Drop body），再 finish 原 Active（字段析构）。
        drop(self);
        outcome
    }
}

fn exact_target_send_error(error: ExactTargetError) -> StrictResponseSendError {
    match error {
        ExactTargetError::TargetGone => StrictResponseSendError::TargetGone,
        ExactTargetError::TargetChanged => StrictResponseSendError::TargetChanged,
    }
}
