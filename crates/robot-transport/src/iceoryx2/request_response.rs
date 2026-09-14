//! 本方显式 profile 的严格单 server / 唯一回复 RR 封装。
//!
//! 保持原 topic、请求 schema attribute、L/C 和 fire-and-forget 配置。
//! 已入队请求始终返回持有原连接的 Pending，投递 issue 只能作诊断；
//! 正常 Closed 后仍须补收尾回复，Unknown 不能作为退休凭证。
//! 非实时线程使用，RT thread 不进入 iceoryx2。

// 中文 doc 块大量出现 `RequestResponse` / `QoS` / `FrameHeader` /
// `RequestPayload` / `MaybeUninit` 等 identifier-like 字串；A1-1 spike
// scope 下不强求 doc-markdown 严格化。
#![allow(clippy::doc_markdown)]

use core::marker::PhantomData;
#[cfg(feature = "test-rr-fixtures")]
use iceoryx2::strict::test_support::RequestQueueFixture;
#[cfg(feature = "test-rr-fixtures")]
#[doc(hidden)]
pub use iceoryx2::strict::test_support::{
    FixtureError, QueueFailure, QueueFixtureSnapshot, SenderCleanupRecord,
};
#[cfg(feature = "test-rr-fixtures")]
use std::cell::RefCell;

/// 真实辅助 A 的非空非法业务载荷；接收夹具只能识别后 Drop，禁止交业务处理器。
#[cfg(feature = "test-rr-fixtures")]
#[doc(hidden)]
pub const REQUEST_QUEUE_FIXTURE_AUXILIARY: &[u8] = b"R3_RR_AUXILIARY_NOT_A_BUSINESS_REQUEST";

use iceoryx2::identifiers::UniqueClientId;
use iceoryx2::port::LoanError;
use iceoryx2::prelude::{
    AllocationStrategy, BackpressureStrategy, MessagingPattern, Node, PortFactory, Service,
};
use iceoryx2::response::Response as IxResponse;
use iceoryx2::service::ServiceDetailsError;
use iceoryx2::service::builder::request_response::{
    RequestResponseCreateError, RequestResponseOpenError, RequestResponseOpenOrCreateError,
};
use iceoryx2::service::ipc::Service as IxService;
use iceoryx2::service::static_config::StaticConfig;
pub use iceoryx2::strict::{EnqueueIssue, StrictChannelStatus};
use iceoryx2::strict::{
    StrictActiveRequest as IxActiveRequest, StrictClient as IxClient, StrictLoanError,
    StrictPendingResponse as IxPendingResponse, StrictSendRefusal, StrictServer as IxServer,
};
use robot_msg::markers::BodyMarker;
use robot_msg::size_tier::default_tier_of;

use crate::error::TransportError;
use crate::iceoryx2::attributes::schema_attribute_verifier;
use crate::iceoryx2::service_name::topic_to_service_name;
use crate::iceoryx2::{CREATE_RACE_MAX_ATTEMPTS, CREATE_RACE_RETRY_INTERVAL};

// 别名跟 publish_subscribe 风格对齐：服务侧 payload + UserHeader 都是
// `[u8]` + `()`。`PendingResponse` / `ActiveRequest` 是 5 个泛型（service +
// req payload + req header + resp payload + resp header），无生命周期；
// `Response` 是 3 个泛型（service + resp payload + resp header）。
type IxRrClient = IxClient<IxService, [u8], (), [u8], ()>;
type IxRrServer = IxServer<IxService, [u8], (), [u8], ()>;
type IxRrPending = IxPendingResponse<IxService, [u8], (), [u8], ()>;
type IxRrActive = IxActiveRequest<IxService, [u8], (), [u8], ()>;
type IxRrResponse = IxResponse<IxService, [u8], ()>;
type IxRrServiceFactory =
    iceoryx2::service::port_factory::request_response::PortFactory<IxService, [u8], (), [u8], ()>;

/// 显式 RR 静态配置；只允许调整每客户端在途上限和客户端上限。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RrProfile {
    pub l: usize,
    pub c: usize,
    pub max_servers: usize,
    pub safe_overflow_requests: bool,
    pub fire_and_forget: bool,
}

impl RrProfile {
    /// S0 控制通道的显式默认值。
    pub const S0_CONTROL: Self = Self {
        l: 4,
        c: 8,
        max_servers: 1,
        safe_overflow_requests: false,
        fire_and_forget: true,
    };

    /// S1 任务通道的显式默认值，独立命名供调用方选择。
    pub const S1_TASK: Self = Self::S0_CONTROL;

    /// 创建前校验固定策略与受检容量；不把零客户端下传给上游归一化。
    ///
    /// # Errors
    ///
    /// 策略不符、L < 3、C 不在 1..=16 或受检乘积大于 64 时拒绝。
    pub fn validate(&self) -> Result<RrStaticLimits, TransportError> {
        check_rr_field("max_servers", 1, self.max_servers as u64)?;
        check_rr_field(
            "enable_safe_overflow_for_requests",
            0,
            u64::from(self.safe_overflow_requests),
        )?;
        check_rr_field(
            "enable_fire_and_forget_requests",
            1,
            u64::from(self.fire_and_forget),
        )?;
        if self.c == 0 {
            return Err(rr_mismatch("max_clients", 1, 0));
        }
        if self.l < 3 {
            return Err(rr_mismatch("derived_limits", 3, self.l as u64));
        }
        if self.c > 16 {
            return Err(rr_mismatch("derived_limits", 16, self.c as u64));
        }
        match self.c.checked_mul(self.l) {
            Some(total) if total <= 64 => Ok(RrStaticLimits {
                l: self.l,
                c: self.c,
            }),
            Some(total) => Err(rr_mismatch("derived_limits", 64, total as u64)),
            // 乘积不可表示，保留导致溢出的真实 L 输入，不能伪造乘积或填零。
            None => Err(rr_mismatch("derived_limits", 64, self.l as u64)),
        }
    }
}

/// 已校验静态容量；实际打开的 service 必须与这两个值精确相等。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RrStaticLimits {
    pub l: usize,
    pub c: usize,
}

impl RrStaticLimits {
    /// 与 profile 的公开校验入口共用同一条受检路径。
    ///
    /// # Errors
    ///
    /// 同 [`RrProfile::validate`]。
    pub fn from_profile(profile: &RrProfile) -> Result<Self, TransportError> {
        profile.validate()
    }
}

/// 请求 loan 尚未投递时的失败原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoanKind {
    OutOfMemory,
    ExceedsMaxLoans,
    ExceedsMaxLoanSize,
    InternalFailure,
}

/// transport 能证明请求尚未入队的失败；底层严格拒绝保留原 typed 原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotDelivered {
    ExceedsMaxActiveRequests,
    Loan(LoanKind),
    RequestIdExhausted,
    NoChannelAvailable,
    ZeroRecipients,
    Strict(StrictSendRefusal),
    #[cfg(feature = "test-rr-fixtures")]
    Fixture(FixtureError),
}

/// 仅表示真正未入队；已入队后的问题由原 Pending 的 delivery_issue 借用观察。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RrSendError {
    NotDelivered(NotDelivered),
}

/// transport 的可信客户端身份；不依赖请求载荷声明的来源。
#[derive(Debug, Eq, Hash, PartialEq, Clone, Copy, PartialOrd, Ord)]
pub struct RrPeerId(UniqueClientId);

fn rr_mismatch(field: &'static str, expected: u64, actual: u64) -> TransportError {
    TransportError::RrConfigMismatch {
        field,
        expected,
        actual,
    }
}

fn check_rr_field(field: &'static str, expected: u64, actual: u64) -> Result<(), TransportError> {
    if expected == actual {
        Ok(())
    } else {
        Err(rr_mismatch(field, expected, actual))
    }
}

/// RR client wrapper（thin）。
///
/// 跟 [`crate::iceoryx2::Iceoryx2Publisher`] 类似的 marker 泛型设计：
/// 类型参数 `Req` / `Resp` 都是 `BodyMarker`，编译期固定 wire schema +
/// 调用方意图；运行时 schema 守门走 service attribute verifier。
pub struct Iceoryx2RrClient<Req: BodyMarker, Resp: BodyMarker> {
    #[cfg(feature = "test-rr-fixtures")]
    request_queue_fixture: RefCell<Option<(RequestQueueFixture, QueueFailure)>>,
    inner: IxRrClient,
    limits: Option<RrStaticLimits>,
    _ty: PhantomData<(Req, Resp)>,
}

impl<Req: BodyMarker, Resp: BodyMarker> core::fmt::Debug for Iceoryx2RrClient<Req, Resp> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Iceoryx2RrClient")
            .field("request_schema", &Req::SCHEMA_NAME)
            .field("response_schema", &Resp::SCHEMA_NAME)
            .finish()
    }
}

/// RR server wrapper（thin）。设计同 [`Iceoryx2RrClient`]。
pub struct Iceoryx2RrServer<Req: BodyMarker, Resp: BodyMarker> {
    inner: IxRrServer,
    limits: Option<RrStaticLimits>,
    _ty: PhantomData<(Req, Resp)>,
}

impl<Req: BodyMarker, Resp: BodyMarker> core::fmt::Debug for Iceoryx2RrServer<Req, Resp> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Iceoryx2RrServer")
            .field("request_schema", &Req::SCHEMA_NAME)
            .field("response_schema", &Resp::SCHEMA_NAME)
            .finish()
    }
}

/// 已入队请求的原 Pending。Drop 仅转交严格内核的 Detached 槽并发送提示；
/// 原槽、连接和引用须等待正常关闭且排空，Unknown 保留到端点真实退出。
pub struct Iceoryx2PendingResponse<Resp: BodyMarker> {
    inner: IxRrPending,
    _ty: PhantomData<Resp>,
}

impl<Resp: BodyMarker> core::fmt::Debug for Iceoryx2PendingResponse<Resp> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Iceoryx2PendingResponse")
            .field("response_schema", &Resp::SCHEMA_NAME)
            .finish()
    }
}

/// `Iceoryx2RrServer::try_recv` 返回的 active request handle。`payload` 借
/// 用 server 内部 sample buffer，零拷贝。`send_response(bytes)` 完成
/// server → client 回包，handle 即消费。
pub struct Iceoryx2ActiveRequest<Req: BodyMarker, Resp: BodyMarker> {
    inner: IxRrActive,
    _ty: PhantomData<(Req, Resp)>,
}

impl<Req: BodyMarker, Resp: BodyMarker> core::fmt::Debug for Iceoryx2ActiveRequest<Req, Resp> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Iceoryx2ActiveRequest")
            .field("request_schema", &Req::SCHEMA_NAME)
            .field("response_schema", &Resp::SCHEMA_NAME)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// service open / client / server 工厂
// ---------------------------------------------------------------------------

/// 使用显式容量与 DiscardData 策略创建客户端。
///
/// # Errors
///
/// profile 非法、既有服务五项静态值不完全一致或后端创建失败时拒绝。
pub fn rr_client_with<Req: BodyMarker, Resp: BodyMarker>(
    node: &Node<IxService>,
    topic: &str,
    profile: &RrProfile,
) -> Result<Iceoryx2RrClient<Req, Resp>, TransportError> {
    let (service, limits) = build_rr_service_with::<Req>(node, topic, profile)?;
    let max_bytes = max_bytes_for::<Req, Resp>()?;
    let client = service
        .client_builder()
        .initial_max_slice_len(max_bytes)
        .allocation_strategy(AllocationStrategy::Static)
        .backpressure_strategy(BackpressureStrategy::DiscardData)
        .create_single_server_strict()
        .map_err(|e| TransportError::Backend(format!("iceoryx2 RR client create: {e:?}")))?;
    Ok(Iceoryx2RrClient {
        #[cfg(feature = "test-rr-fixtures")]
        request_queue_fixture: RefCell::new(None),
        inner: client,
        limits: Some(limits),
        _ty: PhantomData,
    })
}

/// 使用显式容量创建服务端，不移除或重建不匹配的既有服务。
///
/// # Errors
///
/// 同 [`rr_client_with`]。
pub fn rr_server_with<Req: BodyMarker, Resp: BodyMarker>(
    node: &Node<IxService>,
    topic: &str,
    profile: &RrProfile,
) -> Result<Iceoryx2RrServer<Req, Resp>, TransportError> {
    let (service, limits) = build_rr_service_with::<Req>(node, topic, profile)?;
    let max_bytes = max_bytes_for::<Req, Resp>()?;
    let server = service
        .server_builder()
        .initial_max_slice_len(max_bytes)
        .allocation_strategy(AllocationStrategy::Static)
        .backpressure_strategy(BackpressureStrategy::DiscardData)
        .create_single_reply_strict()
        .map_err(|e| TransportError::Backend(format!("iceoryx2 RR server create: {e:?}")))?;
    Ok(Iceoryx2RrServer {
        inner: server,
        limits: Some(limits),
        _ty: PhantomData,
    })
}

fn check_rr_static_config(
    profile: &RrProfile,
    actual: &iceoryx2::service::static_config::request_response::StaticConfig,
) -> Result<(), TransportError> {
    for (field, expected, actual) in [
        (
            "max_active_requests_per_client",
            profile.l as u64,
            actual.max_active_requests_per_client() as u64,
        ),
        ("max_clients", profile.c as u64, actual.max_clients() as u64),
        (
            "max_servers",
            profile.max_servers as u64,
            actual.max_servers() as u64,
        ),
        (
            "enable_safe_overflow_for_requests",
            u64::from(profile.safe_overflow_requests),
            u64::from(actual.has_safe_overflow_for_requests()),
        ),
        (
            "enable_fire_and_forget_requests",
            u64::from(profile.fire_and_forget),
            u64::from(actual.does_support_fire_and_forget_requests()),
        ),
    ] {
        check_rr_field(field, expected, actual)?;
    }
    Ok(())
}

fn is_profile_open_error(err: RequestResponseOpenError) -> bool {
    matches!(
        err,
        RequestResponseOpenError::DoesNotSupportRequestedAmountOfActiveRequestsPerClient
            | RequestResponseOpenError::DoesNotSupportRequestedAmountOfClients
            | RequestResponseOpenError::DoesNotSupportRequestedAmountOfServers
            | RequestResponseOpenError::IncompatibleOverflowBehaviorForRequests
            | RequestResponseOpenError::IncompatibleBehaviorForFireAndForgetRequests
    )
}

fn diagnose_open_error(
    err: RequestResponseOpenError,
    requested: &RrProfile,
    details: Result<Option<StaticConfig>, ServiceDetailsError>,
) -> TransportError {
    let config = match details {
        Ok(Some(config)) => config,
        unavailable @ (Ok(None) | Err(_)) => {
            return TransportError::Backend(format!(
                "iceoryx2 RR open {err:?}; static details unavailable: {unavailable:?}"
            ));
        }
    };
    let actual = match config.messaging_pattern() {
        iceoryx2::service::static_config::messaging_pattern::MessagingPattern::RequestResponse(
            config,
        ) => config,
        pattern => {
            return TransportError::Backend(format!(
                "iceoryx2 RR open {err:?}; static details incompatible: {pattern:?}"
            ));
        }
    };
    let (field, expected, actual) = match err {
        RequestResponseOpenError::DoesNotSupportRequestedAmountOfActiveRequestsPerClient => (
            "max_active_requests_per_client",
            requested.l as u64,
            actual.max_active_requests_per_client() as u64,
        ),
        RequestResponseOpenError::DoesNotSupportRequestedAmountOfClients => (
            "max_clients",
            requested.c as u64,
            actual.max_clients() as u64,
        ),
        RequestResponseOpenError::DoesNotSupportRequestedAmountOfServers => (
            "max_servers",
            requested.max_servers as u64,
            actual.max_servers() as u64,
        ),
        RequestResponseOpenError::IncompatibleOverflowBehaviorForRequests => (
            "enable_safe_overflow_for_requests",
            u64::from(requested.safe_overflow_requests),
            u64::from(actual.has_safe_overflow_for_requests()),
        ),
        RequestResponseOpenError::IncompatibleBehaviorForFireAndForgetRequests => (
            "enable_fire_and_forget_requests",
            u64::from(requested.fire_and_forget),
            u64::from(actual.does_support_fire_and_forget_requests()),
        ),
        _ => return TransportError::Backend(format!("iceoryx2 RR open {err:?}")),
    };
    // 布尔不兼容必为请求值的反值；若详情不再一致，保留打开失败而不伪造 actual。
    // 数值相等同样可能是 open 与只读详情之间发生变化，不能声称配置不匹配。
    if actual == expected {
        TransportError::Backend(format!(
            "iceoryx2 RR open {err:?}; static details no longer show the mismatch"
        ))
    } else {
        rr_mismatch(field, expected, actual)
    }
}

fn build_rr_service_with<Req: BodyMarker>(
    node: &Node<IxService>,
    topic: &str,
    profile: &RrProfile,
) -> Result<(IxRrServiceFactory, RrStaticLimits), TransportError> {
    let limits = profile.validate()?;
    let service_name = topic_to_service_name(topic)?;
    let _tier = default_tier_of(Req::SCHEMA_ID).ok_or_else(|| TransportError::Backend(format!(
        "request schema {:#010x} ({}) has no size tier in robot-msg::size_tier; refusing to create iceoryx2 RR service",
        Req::SCHEMA_ID, Req::SCHEMA_NAME,
    )))?;
    let verifier = schema_attribute_verifier(Req::SCHEMA_ID, 1)?;
    let mut attempt: u32 = 0;
    loop {
        match node
            .service_builder(&service_name)
            .request_response::<[u8], [u8]>()
            .max_active_requests_per_client(profile.l)
            .max_clients(profile.c)
            .max_servers(profile.max_servers)
            .enable_safe_overflow_for_requests(profile.safe_overflow_requests)
            .enable_fire_and_forget_requests(profile.fire_and_forget)
            .open_or_create_with_attributes(&verifier)
        {
            Ok(service) => {
                check_rr_static_config(profile, service.static_config())?;
                return Ok((service, limits));
            }
            Err(e) if attempt < CREATE_RACE_MAX_ATTEMPTS && is_transient_rr_race(e) => {
                attempt += 1;
                std::thread::sleep(CREATE_RACE_RETRY_INTERVAL);
            }
            Err(RequestResponseOpenOrCreateError::RequestResponseOpenError(err))
                if is_profile_open_error(err) =>
            {
                let details = IxService::details(
                    &service_name,
                    node.config(),
                    MessagingPattern::RequestResponse,
                )
                .map(|details| details.map(|details| details.static_details));
                return Err(diagnose_open_error(err, profile, details));
            }
            Err(e) => {
                return Err(TransportError::Backend(format!(
                    "iceoryx2 RR service open_or_create: {e:?} (attempts={})",
                    attempt + 1,
                )));
            }
        }
    }
}

/// 算 client / server `initial_max_slice_len`：取 Req / Resp 两端 size_tier
/// max_bytes 的最大值（spike 简化：两端共用一个上限）。
fn max_bytes_for<Req: BodyMarker, Resp: BodyMarker>() -> Result<usize, TransportError> {
    let req_tier = default_tier_of(Req::SCHEMA_ID).ok_or_else(|| {
        TransportError::Backend(format!(
            "request schema {:#010x} ({}) has no size tier",
            Req::SCHEMA_ID,
            Req::SCHEMA_NAME,
        ))
    })?;
    let resp_tier = default_tier_of(Resp::SCHEMA_ID).ok_or_else(|| {
        TransportError::Backend(format!(
            "response schema {:#010x} ({}) has no size tier",
            Resp::SCHEMA_ID,
            Resp::SCHEMA_NAME,
        ))
    })?;
    Ok(req_tier.max_bytes().max(resp_tier.max_bytes()))
}

/// 判定 RR `open_or_create` 错误是否为并发创建竞态的瞬态错（可重试）。
/// 分类原则同 mod.rs `is_transient_pubsub_race`（穷举 match 锁上游漂移）。
fn is_transient_rr_race(e: RequestResponseOpenOrCreateError) -> bool {
    match e {
        RequestResponseOpenOrCreateError::SystemInFlux => true,
        RequestResponseOpenOrCreateError::RequestResponseOpenError(oe) => matches!(
            oe,
            RequestResponseOpenError::DoesNotExist
                | RequestResponseOpenError::ServiceInCorruptedState
                | RequestResponseOpenError::HangsInCreation
                | RequestResponseOpenError::IsMarkedForDestruction
        ),
        RequestResponseOpenOrCreateError::RequestResponseCreateError(ce) => matches!(
            ce,
            RequestResponseCreateError::AlreadyExists
                | RequestResponseCreateError::IsBeingCreatedByAnotherInstance
                | RequestResponseCreateError::HangsInCreation
                | RequestResponseCreateError::ServiceInCorruptedState
        ),
    }
}

// ---------------------------------------------------------------------------
// Iceoryx2RrClient API
// ---------------------------------------------------------------------------

impl<Req: BodyMarker, Resp: BodyMarker> Iceoryx2RrClient<Req, Resp> {
    /// 返回显式 profile 工厂已经校验的容量。
    #[must_use]
    pub const fn limits(&self) -> Option<RrStaticLimits> {
        self.limits
    }

    /// 唯一发送路径：真正未入队时归还原 request，已入队含 issue 仍返回原 Pending。
    pub fn send_typed(&self, bytes: &[u8]) -> Result<Iceoryx2PendingResponse<Resp>, RrSendError> {
        let request = self
            .inner
            .loan_slice_uninit(bytes.len())
            .map_err(map_strict_loan_error)?;
        let request = request.write_from_slice(bytes);
        #[cfg(feature = "test-rr-fixtures")]
        {
            let mut installed = self.request_queue_fixture.try_borrow_mut().map_err(|_| {
                RrSendError::NotDelivered(NotDelivered::Fixture(FixtureError::Boundary(
                    "原发送夹具正在借用",
                )))
            })?;
            if let Some((fixture, mode)) = installed.as_mut() {
                fixture
                    .before_send(&request, *mode)
                    .map_err(|error| RrSendError::NotDelivered(NotDelivered::Fixture(error)))?;
            }
        }
        let pending = request.send().map_err(|refused| {
            let reason = map_strict_send_refusal(refused.reason);
            // publication 前拒绝，归还同一原 loan/channel，不产生远端责任。
            drop(refused.request);
            reason
        })?;
        Ok(Iceoryx2PendingResponse {
            inner: pending,
            _ty: PhantomData,
        })
    }

    /// 与 send_typed 共用严格发送路径；返回值始终持有真实已入队的原 Pending。
    pub fn send(&self, bytes: &[u8]) -> Result<Iceoryx2PendingResponse<Resp>, TransportError> {
        self.send_typed(bytes)
            .map_err(|error| TransportError::Backend(format!("RR request 未入队: {error:?}")))
    }

    /// 仅显式验收 feature 可安装一次原队列辅助件；没有第二条发送路径。
    #[cfg(feature = "test-rr-fixtures")]
    #[doc(hidden)]
    pub fn install_request_queue_fixture(&self, mode: QueueFailure) -> Result<(), FixtureError> {
        let mut installed = self
            .request_queue_fixture
            .try_borrow_mut()
            .map_err(|_| FixtureError::Boundary("原发送夹具正在借用"))?;
        if installed.is_some() {
            return Err(FixtureError::Boundary("原发送夹具不能重复安装"));
        }
        let fixture = RequestQueueFixture::seed(&self.inner, REQUEST_QUEUE_FIXTURE_AUXILIARY)?;
        *installed = Some((fixture, mode));
        Ok(())
    }

    /// 原连接的纯值快照，不泄露共享内存、offset、pin 或额外持有者。
    #[cfg(feature = "test-rr-fixtures")]
    #[doc(hidden)]
    pub fn request_queue_fixture_snapshot(&self) -> Result<QueueFixtureSnapshot, FixtureError> {
        let installed = self
            .request_queue_fixture
            .try_borrow()
            .map_err(|_| FixtureError::Boundary("原发送夹具正在借用"))?;
        let (fixture, _) = installed
            .as_ref()
            .ok_or(FixtureError::Boundary("原发送夹具未安装"))?;
        fixture.snapshot()
    }
}

// ---------------------------------------------------------------------------
// Iceoryx2PendingResponse API
// ---------------------------------------------------------------------------

impl<Resp: BodyMarker> Iceoryx2PendingResponse<Resp> {
    /// 借用观察真实入队回执的问题，不转移或释放原 Pending。
    #[must_use]
    pub fn delivery_issue(&self) -> Option<&EnqueueIssue> {
        self.inner.delivery_issue()
    }

    /// 按原 pin 查询三态；Closed 后还须补收，Unknown/Open 不证明原请求退休。
    #[must_use]
    pub fn connection_state(&self) -> StrictChannelStatus {
        self.inner.connection_state()
    }

    /// 非阻塞 receive。`Ok(Some(bytes))` = 拿到 reply（拷贝出 `Vec<u8>` 让调
    /// 用方持有；spike scope 简化，A1-3 production 可零拷贝复用 borrow）。
    /// `Ok(None)` = 还没 reply，下轮再 poll。
    ///
    /// # Errors
    ///
    /// iceoryx2 `receive` 错误（连接断 / 服务退出）。
    pub fn try_recv(&self) -> Result<Option<Vec<u8>>, TransportError> {
        match self.inner.receive() {
            Ok(Some(resp)) => Ok(Some(copy_response_payload(&resp))),
            Ok(None) => Ok(None),
            Err(e) => Err(TransportError::Backend(format!(
                "RR pending receive: {e:?}"
            ))),
        }
    }
}

fn copy_response_payload(resp: &IxRrResponse) -> Vec<u8> {
    let payload: &[u8] = resp.payload();
    payload.to_vec()
}

// ---------------------------------------------------------------------------
// Iceoryx2RrServer API
// ---------------------------------------------------------------------------

impl<Req: BodyMarker, Resp: BodyMarker> Iceoryx2RrServer<Req, Resp> {
    /// 返回显式 profile 工厂已经校验的容量。
    #[must_use]
    pub const fn limits(&self) -> Option<RrStaticLimits> {
        self.limits
    }

    /// 非阻塞 receive 一条 active request。`Ok(Some(handle))` = 拿到一条；
    /// `Ok(None)` = 队列空，下轮再 poll。
    ///
    /// **handle 生命周期**（A1-3 (1/3c) round 1 spike 锁住）：
    /// [`Iceoryx2ActiveRequest`] 是**独立 owned** handle，类型签名上**没有**
    /// lifetime 参数。server 可以**同时 hold 多个 active**，反序
    /// `send_response` 仍正确（server 内部按 connection-pair 路由 reply 回
    /// 对应 client `Iceoryx2PendingResponse`，不依赖 `try_recv` 调用顺序）。
    /// 锁测 [`tests::rr_server_can_hold_multiple_active_requests_concurrently`]
    /// 直接验证此行为，A1-3 (1/3c-1) `rr_server_thread` 用
    /// `VecDeque<PendingRr>` 容纳多个 in-flight active。
    ///
    /// # Errors
    ///
    /// iceoryx2 `receive` 错误。
    pub fn try_recv(&self) -> Result<Option<Iceoryx2ActiveRequest<Req, Resp>>, TransportError> {
        match self.inner.receive() {
            Ok(Some(active)) => Ok(Some(Iceoryx2ActiveRequest {
                inner: active,
                _ty: PhantomData,
            })),
            Ok(None) => Ok(None),
            Err(e) => Err(TransportError::Backend(format!("RR server receive: {e:?}"))),
        }
    }
}

// ---------------------------------------------------------------------------
// Iceoryx2ActiveRequest API
// ---------------------------------------------------------------------------

impl<Req: BodyMarker, Resp: BodyMarker> Iceoryx2ActiveRequest<Req, Resp> {
    /// 仅验收时对本原 Active 的响应 sender 执行真实 abandon/remove。
    #[cfg(feature = "test-rr-fixtures")]
    #[doc(hidden)]
    pub fn fixture_abandon_response_sender(&self) -> Result<SenderCleanupRecord, FixtureError> {
        self.inner.fixture_abandon_response_sender()
    }
    /// 来自上游端口的可信来源身份，不读取 payload 内的声明。
    #[must_use]
    pub fn peer(&self) -> RrPeerId {
        RrPeerId(self.inner.origin())
    }

    /// 借用读 request payload bytes（zero-copy；borrow lifetime 绑到 self）。
    #[must_use]
    pub fn payload(&self) -> &[u8] {
        self.inner.payload()
    }

    /// 发送一条 reply 并消费 active request handle。`bytes` 走 `robot-msg`
    /// 的 `build_*_response` 之类构造。
    ///
    /// # Errors
    ///
    /// loan 或发送拒绝，以及回复已入队后附带的回收诊断都会返回错误。
    /// 无论结果如何，self 与唯一 Active 都已消费；不得依据 Err 重发或推断零接纳。
    pub fn send_response(self, bytes: &[u8]) -> Result<(), TransportError> {
        let response = self
            .inner
            .loan_slice_uninit(bytes.len())
            .map_err(|refused| {
                let error = TransportError::Backend(format!(
                    "RR active loan_slice_uninit: {:?}",
                    refused.error
                ));
                // 原 Active 从 loan 拒绝中完整返回；只在本次回复收尾时真实 finish。
                drop(refused.active);
                error
            })?;
        let sent = response
            .write_from_slice(bytes)
            .send()
            .map_err(|error| TransportError::Backend(format!("RR 唯一回复未入队: {error:?}")))?;
        match sent.issue {
            None => Ok(()),
            Some(issue) => Err(TransportError::Backend(format!(
                "RR 唯一回复已入队但附带诊断: {issue:?}"
            ))),
        }
    }
}

fn map_loan_error(error: LoanError) -> LoanKind {
    match error {
        LoanError::OutOfMemory => LoanKind::OutOfMemory,
        LoanError::ExceedsMaxLoans => LoanKind::ExceedsMaxLoans,
        LoanError::ExceedsMaxLoanSize => LoanKind::ExceedsMaxLoanSize,
        LoanError::InternalFailure => LoanKind::InternalFailure,
    }
}

fn map_strict_loan_error(error: StrictLoanError) -> RrSendError {
    RrSendError::NotDelivered(match error {
        StrictLoanError::RequestIdExhausted => NotDelivered::RequestIdExhausted,
        StrictLoanError::NoChannelAvailable => NotDelivered::NoChannelAvailable,
        StrictLoanError::Loan(error) => NotDelivered::Loan(map_loan_error(error)),
    })
}

fn map_strict_send_refusal(error: StrictSendRefusal) -> RrSendError {
    RrSendError::NotDelivered(match error {
        StrictSendRefusal::ExceedsMaxActiveRequests => NotDelivered::ExceedsMaxActiveRequests,
        StrictSendRefusal::NoServer => NotDelivered::ZeroRecipients,
        other => NotDelivered::Strict(other),
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use iceoryx2::prelude::{Config, NodeBuilder};
    use robot_msg::build_helpers::{build_clear_estop_request, build_clear_estop_response};
    use robot_msg::markers;
    use robot_msg::msg::{
        ClearEStopRequest as FbReq, ClearEStopResponse as FbResp, RpcRole, RpcStatus,
    };

    fn fresh_topic(suffix: &str) -> String {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        format!("/rr_spike_test_{}_{}_{suffix}", std::process::id(), seq)
    }

    /// 同进程 RR round-trip：build ClearEStopRequest bytes → client send →
    /// server receive → build ClearEStopResponse bytes → server reply →
    /// client receive → 解析回原始字段。
    ///
    /// **spike 唯一目的**：reviewer A1 第 1 个拍板"跨进程通道选 C2 iceoryx2
    /// RequestResponse" 的最小验证。能跑过即证明方案可用。
    #[test]
    fn round_trip_clear_estop() {
        let node = NodeBuilder::new().create::<IxService>().unwrap();
        let topic = fresh_topic("round_trip");
        let (client, server) = explicit_pair(&node, &topic);

        // build request bytes（A1-1 part 1 落地的 robot-msg helper）
        let req_bytes = build_clear_estop_request(
            "req-spike-001",
            "idem-001",
            "rr_spike_test",
            RpcRole::Maintainer,
            "spike-confirm",
            "rr round-trip spike",
            /* dry_run */ false,
            /* deadline_ns */ 1_234_567_890_000,
            /* stamp_ns */ 1_234_567_000_000,
        );

        // client send
        let pending = client.send(&req_bytes).unwrap();

        // server receive + verify request payload
        let active = wait_for_active(&server, std::time::Duration::from_secs(2));
        let server_view = flatbuffers::root::<FbReq>(active.payload()).unwrap();
        assert_eq!(server_view.request_id().unwrap(), "req-spike-001");
        assert_eq!(server_view.role(), RpcRole::Maintainer);
        assert!(!server_view.dry_run());

        // server build response bytes + reply
        let resp_bytes = build_clear_estop_response(
            "req-spike-001",
            RpcStatus::Ok,
            "",
            /* cleared_at_ns */ 1_234_567_999_999,
            /* dry_run */ false,
        );
        active.send_response(&resp_bytes).unwrap();

        // client receive + verify response payload
        let resp = wait_for_response(&pending, std::time::Duration::from_secs(2));
        let resp_view = flatbuffers::root::<FbResp>(&resp).unwrap();
        assert_eq!(resp_view.request_id().unwrap(), "req-spike-001");
        assert_eq!(resp_view.status(), RpcStatus::Ok);
        assert_eq!(resp_view.cleared_at_ns(), 1_234_567_999_999);
    }

    /// **A1-3 (1/3c-1) 前置 spike**：server 端**hold 多个 active request**
    /// 后**反序 reply** 仍正确路由。
    ///
    /// `Iceoryx2RrServer::try_recv` 现有 doc 写"`handle` 持 server 借用 →
    /// 在 handle drop 之前不能再 `try_recv`"；但 [`Iceoryx2ActiveRequest`]
    /// 类型签名上**没有** lifetime 参数（`PhantomData<(Req, Resp)>` 而非
    /// `PhantomData<&'srv ...>`）。本测试锁住实际行为：server 可以同时
    /// hold 多个 active；reply 顺序跟 try_recv 顺序无关；request_id
    /// correlation 由 wire 字段保证（不靠 active handle 的 ordering）。
    ///
    /// 锁住后 A1-3 (1/3c-1) `rr_server_thread` 可以用 `VecDeque<PendingRr>`
    /// 容纳多个 in-flight active；如果本测试失败，切回 `Option<PendingRr>`
    /// 单 active 模式（reviewer A1-3 (1/3c) round 1 拍板要求）。
    #[test]
    fn rr_server_can_hold_multiple_active_requests_concurrently() {
        let node = NodeBuilder::new().create::<IxService>().unwrap();
        let topic = fresh_topic("multi_active");
        let (client, server) = explicit_pair(&node, &topic);

        let req_a = build_clear_estop_request(
            "req-A",
            "idem-A",
            "src",
            RpcRole::Maintainer,
            "confirm-A",
            "audit-A",
            false,
            1_000_000,
            1_000,
        );
        let req_b = build_clear_estop_request(
            "req-B",
            "idem-B",
            "src",
            RpcRole::Maintainer,
            "confirm-B",
            "audit-B",
            false,
            2_000_000,
            2_000,
        );
        let pending_a = client.send(&req_a).unwrap();
        let pending_b = client.send(&req_b).unwrap();

        // 关键：连续 try_recv 两条 active，第二条调用时第一条仍未 drop
        // 也未 send_response。
        let active_a = wait_for_active(&server, std::time::Duration::from_secs(2));
        let active_b = wait_for_active(&server, std::time::Duration::from_secs(2));

        // 两个 active 持不同 request_id（顺序不预设，因 iceoryx2 RR 不保证
        // FIFO；按 request_id 双向匹配）
        let a_id = flatbuffers::root::<FbReq>(active_a.payload())
            .unwrap()
            .request_id()
            .unwrap_or("")
            .to_string();
        let b_id = flatbuffers::root::<FbReq>(active_b.payload())
            .unwrap()
            .request_id()
            .unwrap_or("")
            .to_string();
        let mut got = [a_id.clone(), b_id.clone()];
        got.sort();
        assert_eq!(
            got,
            ["req-A", "req-B"],
            "两个 active 应分别持 req-A 和 req-B（hold 多个不丢失 wire payload）"
        );

        // 反序 reply：active_b 先 reply，active_a 后 reply。
        // active_a 的 request_id 决定它对应哪条 client pending（不是 try_recv 顺序）。
        let resp_for_b = build_clear_estop_response(&b_id, RpcStatus::Ok, "", 2_999_999, false);
        active_b.send_response(&resp_for_b).unwrap();
        let resp_for_a =
            build_clear_estop_response(&a_id, RpcStatus::FailedPrecondition, "gate-fail", 0, false);
        active_a.send_response(&resp_for_a).unwrap();

        // client 端按 wire request_id 字段验证 response 路由正确。
        // (iceoryx2 RR pending_response 走 connection-pair 路由，不是 wire
        //  字段；wire request_id 仅在 server 内部分发用。client 端 pending_a
        //  无论如何只能收 server 端 active_a.send_response 的 bytes。本断言
        //  校 server 端按 a_id 正确构 wire response。)
        let bytes_for_a = wait_for_response(&pending_a, std::time::Duration::from_secs(2));
        let view_for_a = flatbuffers::root::<FbResp>(&bytes_for_a).unwrap();
        let bytes_for_b = wait_for_response(&pending_b, std::time::Duration::from_secs(2));
        let view_for_b = flatbuffers::root::<FbResp>(&bytes_for_b).unwrap();
        // 两条 response request_id 跟两个 pending 的 wire request_id 字段一致
        let mut resp_pair = [
            view_for_a.request_id().unwrap_or("").to_string(),
            view_for_b.request_id().unwrap_or("").to_string(),
        ];
        resp_pair.sort();
        assert_eq!(resp_pair, ["req-A", "req-B"]);
    }

    /// 反向 sanity check：用错的 schema marker 打开同一 topic → service
    /// attribute verifier 拒绝。证明 attribute 守门生效，C2 跟现有 schema
    /// attribute 双道防线兼容。
    #[test]
    fn schema_attribute_mismatch_rejects_open() {
        let node = NodeBuilder::new().create::<IxService>().unwrap();
        let topic = fresh_topic("schema_mismatch");
        // 第一次 create：用 ClearEStopRequest 作 schema attribute
        let _ok = explicit_pair(&node, &topic);
        // 第二次 open：用 Heartbeat 作 schema → attribute verify 失败
        let bad = rr_client_with::<markers::Heartbeat, markers::ClearEStopResponse>(
            &node,
            &topic,
            &RrProfile::S0_CONTROL,
        );
        assert!(
            bad.is_err(),
            "expected schema attribute verify to reject mismatched request marker"
        );
    }

    type TestClient = Iceoryx2RrClient<markers::ClearEStopRequest, markers::ClearEStopResponse>;
    type TestServer = Iceoryx2RrServer<markers::ClearEStopRequest, markers::ClearEStopResponse>;

    fn explicit_pair(node: &Node<IxService>, topic: &str) -> (TestClient, TestServer) {
        let server = rr_server_with(node, topic, &RrProfile::S0_CONTROL).unwrap();
        let client = rr_client_with(node, topic, &RrProfile::S0_CONTROL).unwrap();
        (client, server)
    }

    fn static_details(topic: &str) -> Option<StaticConfig> {
        IxService::details(
            &topic_to_service_name(topic).unwrap(),
            Config::global_config(),
            MessagingPattern::RequestResponse,
        )
        .unwrap()
        .map(|details| details.static_details)
    }

    fn prebuilt(node: &Node<IxService>, topic: &str, profile: RrProfile) -> IxRrServiceFactory {
        let name = topic_to_service_name(topic).unwrap();
        let attributes =
            crate::iceoryx2::schema_attribute_spec(markers::ClearEStopRequest::SCHEMA_ID, 1)
                .unwrap();
        node.service_builder(&name)
            .request_response::<[u8], [u8]>()
            .max_active_requests_per_client(profile.l)
            .max_clients(profile.c)
            .max_servers(profile.max_servers)
            .enable_safe_overflow_for_requests(profile.safe_overflow_requests)
            .enable_fire_and_forget_requests(profile.fire_and_forget)
            .create_with_attributes(&attributes)
            .unwrap()
    }

    fn assert_existing_rejected(profile: RrProfile, expected: &TransportError) {
        let node = NodeBuilder::new().create::<IxService>().unwrap();
        let topic = fresh_topic("existing_profile");
        let factory = prebuilt(&node, &topic, profile);
        let before = static_details(&topic).unwrap();
        check_rr_static_config(&profile, factory.static_config()).unwrap();
        check_rr_static_config(&profile, before.request_response()).unwrap();
        let result = rr_client_with::<markers::ClearEStopRequest, markers::ClearEStopResponse>(
            &node,
            &topic,
            &RrProfile::S0_CONTROL,
        );
        assert_eq!(&result.unwrap_err(), expected);
        // 在复核后才 drop factory；其实际唯一 ID 证明旧服务一直存在、没有重建。
        let after = static_details(&topic).unwrap();
        assert_eq!(before.unique_service_id(), after.unique_service_id());
        assert_eq!(before.service_hash(), after.service_hash());
        check_rr_static_config(&profile, after.request_response()).unwrap();
        drop(factory);
    }

    #[test]
    fn open_existing_l16_service_is_rejected_with_real_actual() {
        assert_existing_rejected(
            RrProfile {
                l: 16,
                ..RrProfile::S0_CONTROL
            },
            &rr_mismatch("max_active_requests_per_client", 4, 16),
        );
    }

    #[test]
    fn open_existing_l2_service_maps_builder_error_with_real_actual() {
        assert_existing_rejected(
            RrProfile {
                l: 2,
                ..RrProfile::S0_CONTROL
            },
            &rr_mismatch("max_active_requests_per_client", 4, 2),
        );
    }

    #[test]
    fn open_existing_two_servers_is_rejected() {
        assert_existing_rejected(
            RrProfile {
                max_servers: 2,
                ..RrProfile::S0_CONTROL
            },
            &rr_mismatch("max_servers", 1, 2),
        );
    }

    #[test]
    fn open_existing_overflow_on_is_rejected() {
        assert_existing_rejected(
            RrProfile {
                safe_overflow_requests: true,
                ..RrProfile::S0_CONTROL
            },
            &rr_mismatch("enable_safe_overflow_for_requests", 0, 1),
        );
    }

    #[test]
    fn open_existing_fire_and_forget_off_is_rejected() {
        assert_existing_rejected(
            RrProfile {
                fire_and_forget: false,
                ..RrProfile::S0_CONTROL
            },
            &rr_mismatch("enable_fire_and_forget_requests", 1, 0),
        );
    }

    #[test]
    fn open_existing_client_count_is_exact() {
        for c in [2, 16] {
            assert_existing_rejected(
                RrProfile {
                    c,
                    ..RrProfile::S0_CONTROL
                },
                &rr_mismatch("max_clients", 8, c as u64),
            );
        }
    }

    #[test]
    fn policy_fields_rejected_before_any_create() {
        let node = NodeBuilder::new().create::<IxService>().unwrap();
        for (profile, expected) in [
            (
                RrProfile {
                    max_servers: 2,
                    ..RrProfile::S0_CONTROL
                },
                rr_mismatch("max_servers", 1, 2),
            ),
            (
                RrProfile {
                    safe_overflow_requests: true,
                    ..RrProfile::S0_CONTROL
                },
                rr_mismatch("enable_safe_overflow_for_requests", 0, 1),
            ),
            (
                RrProfile {
                    fire_and_forget: false,
                    ..RrProfile::S0_CONTROL
                },
                rr_mismatch("enable_fire_and_forget_requests", 1, 0),
            ),
        ] {
            let topic = fresh_topic("invalid_policy");
            assert!(static_details(&topic).is_none());
            assert_eq!(profile.validate().unwrap_err(), expected);
            assert_eq!(
                RrStaticLimits::from_profile(&profile).unwrap_err(),
                expected
            );
            assert_eq!(
                rr_client_with::<markers::ClearEStopRequest, markers::ClearEStopResponse>(
                    &node, &topic, &profile
                )
                .unwrap_err(),
                expected
            );
            assert_eq!(
                rr_server_with::<markers::ClearEStopRequest, markers::ClearEStopResponse>(
                    &node, &topic, &profile
                )
                .unwrap_err(),
                expected
            );
            assert!(static_details(&topic).is_none());
        }
    }

    #[test]
    fn zero_clients_are_rejected_before_any_create_even_with_maximum_l() {
        let node = NodeBuilder::new().create::<IxService>().unwrap();
        for l in [4, usize::MAX] {
            let profile = RrProfile {
                l,
                c: 0,
                ..RrProfile::S0_CONTROL
            };
            let expected = rr_mismatch("max_clients", 1, 0);
            let topic = fresh_topic("zero_clients");
            assert!(static_details(&topic).is_none());
            assert_eq!(profile.validate().unwrap_err(), expected);
            assert_eq!(
                RrStaticLimits::from_profile(&profile).unwrap_err(),
                expected
            );
            assert_eq!(
                rr_client_with::<markers::ClearEStopRequest, markers::ClearEStopResponse>(
                    &node, &topic, &profile
                )
                .unwrap_err(),
                expected
            );
            assert_eq!(
                rr_server_with::<markers::ClearEStopRequest, markers::ClearEStopResponse>(
                    &node, &topic, &profile
                )
                .unwrap_err(),
                expected
            );
            assert!(static_details(&topic).is_none());
        }
    }

    #[test]
    fn derived_limits_guard() {
        for (l, c, expected, actual) in [
            (2, 8, 3_u64, 2),
            (4, 17, 16, 17),
            (16, 8, 64, 128),
            (65, 1, 64, 65),
            (usize::MAX, 16, 64, usize::MAX),
        ] {
            let profile = RrProfile {
                l,
                c,
                ..RrProfile::S0_CONTROL
            };
            assert_eq!(
                profile.validate(),
                Err(rr_mismatch("derived_limits", expected, actual as u64))
            );
            assert_eq!(RrStaticLimits::from_profile(&profile), profile.validate());
        }
        for (l, c) in [(3, 16), (4, 16), (64, 1), (4, 8)] {
            let profile = RrProfile {
                l,
                c,
                ..RrProfile::S0_CONTROL
            };
            assert_eq!(profile.validate(), Ok(RrStaticLimits { l, c }));
            assert_eq!(RrStaticLimits::from_profile(&profile), profile.validate());
        }
        assert_eq!(RrProfile::S0_CONTROL, RrProfile::S1_TASK);
    }

    #[test]
    fn diagnose_open_error_keeps_upstream_error_when_details_unavailable() {
        let upstream =
            RequestResponseOpenError::DoesNotSupportRequestedAmountOfActiveRequestsPerClient;
        let unavailable = [
            None,
            Some(ServiceDetailsError::FailedToOpenStaticServiceInfo),
            Some(ServiceDetailsError::FailedToReadStaticServiceInfo),
            Some(ServiceDetailsError::FailedToDeserializeStaticServiceInfo),
            Some(ServiceDetailsError::ServiceInInconsistentState),
            Some(ServiceDetailsError::VersionMismatch),
            Some(ServiceDetailsError::InternalError),
            Some(ServiceDetailsError::FailedToAcquireNodeState),
        ];
        for error in unavailable {
            let details = error.map_or(Ok(None), Err);
            let error = diagnose_open_error(upstream, &RrProfile::S0_CONTROL, details);
            match error {
                TransportError::Backend(text) => {
                    assert!(
                        text.contains("DoesNotSupportRequestedAmountOfActiveRequestsPerClient")
                    );
                    assert!(text.contains("static details unavailable"));
                    assert!(!text.contains("actual: 0"));
                }
                other => panic!("必须保留原始打开错误：{other:?}"),
            }
        }
        let node = NodeBuilder::new().create::<IxService>().unwrap();
        let topic = fresh_topic("diagnose_actual");
        let factory = prebuilt(
            &node,
            &topic,
            RrProfile {
                l: 2,
                ..RrProfile::S0_CONTROL
            },
        );
        let config = static_details(&topic).unwrap();
        assert_eq!(
            diagnose_open_error(upstream, &RrProfile::S0_CONTROL, Ok(Some(config))),
            rr_mismatch("max_active_requests_per_client", 4, 2)
        );
        drop(factory);
    }

    #[test]
    fn stale_boolean_diagnosis_keeps_original_open_failure() {
        let node = NodeBuilder::new().create::<IxService>().unwrap();
        let topic = fresh_topic("diagnose_changed");
        let factory = prebuilt(&node, &topic, RrProfile::S0_CONTROL);
        for error in [
            RequestResponseOpenError::IncompatibleOverflowBehaviorForRequests,
            RequestResponseOpenError::IncompatibleBehaviorForFireAndForgetRequests,
        ] {
            let diagnosed =
                diagnose_open_error(error, &RrProfile::S0_CONTROL, Ok(static_details(&topic)));
            assert!(matches!(diagnosed, TransportError::Backend(_)));
        }
        drop(factory);
    }

    #[test]
    fn custom_node_diagnosis_uses_its_own_same_named_service() {
        use iceoryx2::prelude::{FileName, SemanticString};

        let global_node = NodeBuilder::new().create::<IxService>().unwrap();
        let mut config = global_node.config().clone();
        config.global.prefix =
            FileName::new(format!("pt_custom_{}_", std::process::id()).as_bytes()).unwrap();
        assert_ne!(config.global.prefix, global_node.config().global.prefix);
        let custom_node = NodeBuilder::new()
            .config(&config)
            .create::<IxService>()
            .unwrap();
        let topic = fresh_topic("same_name_two_configs");
        let global_profile = RrProfile {
            l: 16,
            ..RrProfile::S0_CONTROL
        };
        let custom_profile = RrProfile {
            l: 2,
            ..RrProfile::S0_CONTROL
        };
        let global_factory = prebuilt(&global_node, &topic, global_profile);
        let custom_factory = prebuilt(&custom_node, &topic, custom_profile);
        let name = topic_to_service_name(&topic).unwrap();
        let details = |config: &Config| {
            IxService::details(&name, config, MessagingPattern::RequestResponse)
                .unwrap()
                .unwrap()
                .static_details
        };
        let global_before = details(global_node.config());
        let custom_before = details(custom_node.config());
        check_rr_static_config(&global_profile, global_before.request_response()).unwrap();
        check_rr_static_config(&custom_profile, custom_before.request_response()).unwrap();
        assert_ne!(
            global_before.unique_service_id(),
            custom_before.unique_service_id()
        );
        let errors = [
            rr_client_with::<markers::ClearEStopRequest, markers::ClearEStopResponse>(
                &custom_node,
                &topic,
                &RrProfile::S0_CONTROL,
            )
            .unwrap_err(),
            rr_server_with::<markers::ClearEStopRequest, markers::ClearEStopResponse>(
                &custom_node,
                &topic,
                &RrProfile::S0_CONTROL,
            )
            .unwrap_err(),
        ];
        let global_after = details(global_node.config());
        let custom_after = details(custom_node.config());
        assert_eq!(
            global_before.unique_service_id(),
            global_after.unique_service_id()
        );
        assert_eq!(
            custom_before.unique_service_id(),
            custom_after.unique_service_id()
        );
        check_rr_static_config(&global_profile, global_after.request_response()).unwrap();
        check_rr_static_config(&custom_profile, custom_after.request_response()).unwrap();
        let expected = rr_mismatch("max_active_requests_per_client", 4, 2);
        assert_eq!(errors, [expected.clone(), expected]);
        drop((global_factory, custom_factory));
    }

    #[test]
    fn client_cap_fifth_send_is_not_delivered() {
        let node = NodeBuilder::new().create::<IxService>().unwrap();
        let (client, server) = explicit_pair(&node, &fresh_topic("fifth"));
        let pending: Vec<_> = (0..4).map(|id| client.send_typed(&[id]).unwrap()).collect();
        assert!(matches!(
            client.send_typed(&[4]),
            Err(RrSendError::NotDelivered(
                NotDelivered::ExceedsMaxActiveRequests
            ))
        ));
        assert_eq!(client.limits(), Some(RrStaticLimits { l: 4, c: 8 }));
        assert_eq!(server.limits(), client.limits());
        assert!(
            pending
                .iter()
                .all(|p| p.connection_state() == StrictChannelStatus::Open
                    && p.delivery_issue().is_none())
        );
    }

    #[test]
    fn early_pending_drop_keeps_original_capacity_until_active_finishes() {
        let node = NodeBuilder::new().create::<IxService>().unwrap();
        let (client, server) = explicit_pair(&node, &fresh_topic("detached_capacity"));
        let pending: Vec<_> = (0..4).map(|id| client.send_typed(&[id]).unwrap()).collect();
        let held: Vec<_> = (0..4)
            .map(|_| wait_for_active(&server, std::time::Duration::from_secs(2)))
            .collect();
        drop(pending);
        assert_eq!(client.inner.number_of_detached_requests(), 4);
        assert_eq!(client.inner.number_of_active_requests(), 4);
        assert!(matches!(
            client.send_typed(&[99]),
            Err(RrSendError::NotDelivered(
                NotDelivered::ExceedsMaxActiveRequests
            ))
        ));
        assert_eq!(client.inner.number_of_detached_requests(), 4);
        drop(held);
        client.inner.maintain();
        assert_eq!(client.inner.number_of_detached_requests(), 0);
        assert_eq!(client.inner.number_of_active_requests(), 0);
        let next: Vec<_> = (10..14)
            .map(|id| client.send_typed(&[id]).unwrap())
            .collect();
        assert_eq!(next.len(), 4);
    }

    #[test]
    fn final_reply_remains_visible_after_whole_server_drop() {
        let node = NodeBuilder::new().create::<IxService>().unwrap();
        let (client, server) = explicit_pair(&node, &fresh_topic("server_drop_tail"));
        let pending = client.send_typed(&[7]).unwrap();
        assert_eq!(pending.connection_state(), StrictChannelStatus::Open);
        assert!(pending.try_recv().unwrap().is_none());
        let active = wait_for_active(&server, std::time::Duration::from_secs(2));
        assert_eq!(active.peer(), RrPeerId(client.inner.id()));
        active.send_response(&[8]).unwrap();
        drop(server);
        assert_eq!(pending.connection_state(), StrictChannelStatus::Closed);
        assert_eq!(pending.try_recv().unwrap(), Some(vec![8]));
        assert_eq!(pending.try_recv().unwrap(), None);
        drop(pending);
        client.inner.maintain();
        assert_eq!(client.inner.number_of_active_requests(), 0);
        assert_eq!(client.inner.number_of_pinned_routes(), 0);
    }

    #[test]
    fn original_response_borrow_delays_detached_reclaim() {
        let node = NodeBuilder::new().create::<IxService>().unwrap();
        let (client, server) = explicit_pair(&node, &fresh_topic("held_response"));
        let pending = client.send_typed(&[10]).unwrap();
        wait_for_active(&server, std::time::Duration::from_secs(2))
            .send_response(&[11])
            .unwrap();
        // 仅同模块测试持有真实 Response，公开 try_recv 仍拷出并释放借用。
        let response = pending.inner.receive().unwrap().unwrap();
        assert_eq!(response.payload(), &[11]);
        drop(pending);
        client.inner.maintain();
        assert_eq!(client.inner.number_of_detached_requests(), 1);
        assert_eq!(client.inner.number_of_pinned_routes(), 1);
        assert_eq!(client.inner.number_of_active_requests(), 1);
        drop(response);
        client.inner.maintain();
        assert_eq!(client.inner.number_of_detached_requests(), 0);
        assert_eq!(client.inner.number_of_pinned_routes(), 0);
        assert_eq!(client.inner.number_of_active_requests(), 0);
    }

    #[test]
    fn original_active_drop_closes_without_a_reply() {
        let node = NodeBuilder::new().create::<IxService>().unwrap();
        let (client, server) = explicit_pair(&node, &fresh_topic("active_drop"));
        let pending = client.send_typed(&[20]).unwrap();
        let active = wait_for_active(&server, std::time::Duration::from_secs(2));
        assert_eq!(pending.connection_state(), StrictChannelStatus::Open);
        drop(active);
        assert_eq!(pending.connection_state(), StrictChannelStatus::Closed);
        assert_eq!(pending.try_recv().unwrap(), None);
        drop(pending);
        client.inner.maintain();
        assert_eq!(client.inner.number_of_active_requests(), 0);
    }

    #[test]
    fn reply_loan_refusal_finishes_only_the_original_active() {
        let node = NodeBuilder::new().create::<IxService>().unwrap();
        let (client, server) = explicit_pair(&node, &fresh_topic("reply_loan_refusal"));
        let first = client.send_typed(&[30]).unwrap();
        let active = wait_for_active(&server, std::time::Duration::from_secs(2));
        let second = client.send_typed(&[31]).unwrap();
        let other = wait_for_active(&server, std::time::Duration::from_secs(2));
        let oversized = vec![
            0;
            max_bytes_for::<markers::ClearEStopRequest, markers::ClearEStopResponse>().unwrap()
                + 1
        ];
        assert!(active.send_response(&oversized).is_err());
        assert_eq!(first.connection_state(), StrictChannelStatus::Closed);
        assert_eq!(first.try_recv().unwrap(), None);
        assert_eq!(second.connection_state(), StrictChannelStatus::Open);
        other.send_response(&[32]).unwrap();
        assert_eq!(second.try_recv().unwrap(), Some(vec![32]));
        assert_eq!(second.try_recv().unwrap(), None);
    }

    #[test]
    fn absent_server_refuses_before_remote_acceptance() {
        let node = NodeBuilder::new().create::<IxService>().unwrap();
        let client: TestClient =
            rr_client_with(&node, &fresh_topic("absent_server"), &RrProfile::S0_CONTROL).unwrap();
        assert!(matches!(
            client.send_typed(&[1]),
            Err(RrSendError::NotDelivered(NotDelivered::ZeroRecipients))
        ));
        assert_eq!(client.inner.number_of_active_requests(), 0);
        assert_eq!(client.inner.number_of_detached_requests(), 0);
        assert_eq!(client.inner.number_of_pinned_routes(), 0);
    }

    #[test]
    fn peer_id_is_stable_per_client_and_distinct_across_clients() {
        let node = NodeBuilder::new().create::<IxService>().unwrap();
        let topic = fresh_topic("peer");
        let (client, server) = explicit_pair(&node, &topic);
        let other: TestClient = rr_client_with(&node, &topic, &RrProfile::S0_CONTROL).unwrap();
        let first = client.send_typed(&[1]).unwrap();
        let a = wait_for_active(&server, std::time::Duration::from_secs(2));
        let second = client.send_typed(&[2]).unwrap();
        let b = wait_for_active(&server, std::time::Duration::from_secs(2));
        let third = other.send_typed(&[3]).unwrap();
        let c = wait_for_active(&server, std::time::Duration::from_secs(2));
        assert_eq!(a.peer(), b.peer());
        assert_ne!(a.peer(), c.peer());
        let mut peers = std::collections::HashMap::new();
        peers.insert(a.peer(), 1);
        peers.insert(b.peer(), 2);
        peers.insert(c.peer(), 3);
        assert_eq!(peers.len(), 2);
        assert_eq!(peers[&a.peer()], 2);
        drop((first, second, third));
    }

    #[test]
    fn strict_errors_only_describe_requests_not_enqueued() {
        assert_eq!(
            map_strict_send_refusal(StrictSendRefusal::NoServer),
            RrSendError::NotDelivered(NotDelivered::ZeroRecipients)
        );
        assert_eq!(
            map_strict_send_refusal(StrictSendRefusal::ExceedsMaxActiveRequests),
            RrSendError::NotDelivered(NotDelivered::ExceedsMaxActiveRequests)
        );
        for refusal in [
            StrictSendRefusal::TargetMismatch,
            StrictSendRefusal::SlotBusy,
            StrictSendRefusal::MultipleServers { count: 2 },
            StrictSendRefusal::ServerNotStatic,
        ] {
            assert_eq!(
                map_strict_send_refusal(refusal),
                RrSendError::NotDelivered(NotDelivered::Strict(refusal))
            );
        }
        for (error, kind) in [
            (LoanError::OutOfMemory, LoanKind::OutOfMemory),
            (LoanError::ExceedsMaxLoans, LoanKind::ExceedsMaxLoans),
            (LoanError::ExceedsMaxLoanSize, LoanKind::ExceedsMaxLoanSize),
            (LoanError::InternalFailure, LoanKind::InternalFailure),
        ] {
            assert_eq!(
                map_strict_loan_error(StrictLoanError::Loan(error)),
                RrSendError::NotDelivered(NotDelivered::Loan(kind))
            );
        }
        assert_eq!(
            map_strict_loan_error(StrictLoanError::RequestIdExhausted),
            RrSendError::NotDelivered(NotDelivered::RequestIdExhausted)
        );
        assert_eq!(
            map_strict_loan_error(StrictLoanError::NoChannelAvailable),
            RrSendError::NotDelivered(NotDelivered::NoChannelAvailable)
        );
    }

    #[cfg(feature = "test-rr-fixtures")]
    struct QueueFaultOwned {
        server: Option<TestServer>,
        pending: Vec<Iceoryx2PendingResponse<markers::ClearEStopResponse>>,
        client: TestClient,
        _node: Node<IxService>,
    }

    #[cfg(feature = "test-rr-fixtures")]
    impl Drop for QueueFaultOwned {
        fn drop(&mut self) {
            // 本例 server 同线程无 worker；先真正结束原 server，再还原 Pending/A/client。
            self.server.take();
        }
    }

    #[cfg(feature = "test-rr-fixtures")]
    #[test]
    fn real_original_queue_full_refuses_fourth_transport_send() {
        exercise_queue_fault(QueueFailure::NotEnqueued);
    }

    #[cfg(feature = "test-rr-fixtures")]
    #[test]
    fn real_original_queue_issue_keeps_fourth_pending_and_reply() {
        exercise_queue_fault(QueueFailure::EnqueueIssue);
    }

    #[cfg(feature = "test-rr-fixtures")]
    #[allow(
        clippy::too_many_lines,
        reason = "同一原队列的 A/P1/P2/P3/B 预算与回收证据保留在同一拥有域"
    )]
    fn exercise_queue_fault(mode: QueueFailure) {
        use iceoryx2_cal::zero_copy_connection::ZeroCopySendError;

        let node = NodeBuilder::new().create::<IxService>().unwrap();
        let (client, server) = explicit_pair(&node, &fresh_topic("original_queue_fault"));
        let mut owned = QueueFaultOwned {
            server: Some(server),
            pending: Vec::new(),
            client,
            _node: node,
        };
        assert!(owned.server.as_ref().unwrap().try_recv().unwrap().is_none());
        owned.client.install_request_queue_fixture(mode).unwrap();
        assert!(matches!(
            owned.client.install_request_queue_fixture(mode),
            Err(FixtureError::Boundary(_))
        ));
        let initial = owned.client.request_queue_fixture_snapshot().unwrap();
        assert_eq!(
            (
                initial.loans,
                initial.active,
                initial.detached,
                initial.pool_free
            ),
            (1, 0, 0, 9)
        );
        assert_eq!(
            (initial.channels, initial.samples, initial.queue_capacity),
            (10, 10, 4)
        );
        assert_eq!(initial.auxiliary_references, 2);
        assert!(initial.auxiliary_used && !initial.armed && !initial.target_seen);
        assert_eq!(initial.hits, 0);
        for id in 1..=3 {
            owned.pending.push(owned.client.send_typed(&[id]).unwrap());
            assert!(owned.pending.last().unwrap().delivery_issue().is_none());
            let observed = owned.client.request_queue_fixture_snapshot().unwrap();
            assert_eq!(
                (observed.client_id, observed.server_id),
                (initial.client_id, initial.server_id)
            );
            assert_eq!(
                (observed.loans, observed.active, observed.detached),
                (1, usize::from(id), 0)
            );
            assert_eq!(observed.pool_free, 9 - usize::from(id));
            assert_eq!(observed.auxiliary_references, 2);
            assert!(observed.auxiliary_used && !observed.armed && !observed.target_seen);
            assert_eq!(observed.hits, 0);
        }
        let result = owned.client.send_typed(&[4]);
        match mode {
            QueueFailure::NotEnqueued => assert!(matches!(
                result,
                Err(RrSendError::NotDelivered(NotDelivered::Strict(
                    StrictSendRefusal::NotEnqueued(ZeroCopySendError::ReceiveBufferFull)
                )))
            )),
            QueueFailure::EnqueueIssue => {
                owned
                    .pending
                    .push(result.expect("真实入队 issue 必须返回原 Pending"));
                assert!(matches!(
                    owned.pending.last().unwrap().delivery_issue(),
                    Some(EnqueueIssue::OverflowedOffsetNotInUsedList(_))
                ));
            }
        }
        let sent = owned.client.request_queue_fixture_snapshot().unwrap();
        let count = match mode {
            QueueFailure::NotEnqueued => 3,
            QueueFailure::EnqueueIssue => 4,
        };
        assert_eq!(
            (sent.client_id, sent.server_id),
            (initial.client_id, initial.server_id)
        );
        assert_eq!(
            (sent.loans, sent.active, sent.detached, sent.pool_free),
            (1, count, 0, 9 - count)
        );
        assert!(sent.target_seen && !sent.armed);
        assert_eq!(sent.hits, usize::from(mode == QueueFailure::EnqueueIssue));
        assert_eq!(sent.auxiliary_used, mode == QueueFailure::NotEnqueued);
        assert_eq!(sent.auxiliary_references, 2);
        let server = owned.server.as_ref().unwrap();
        if mode == QueueFailure::NotEnqueued {
            let auxiliary = wait_for_active(server, std::time::Duration::from_secs(2));
            assert_eq!(auxiliary.payload(), REQUEST_QUEUE_FIXTURE_AUXILIARY);
            drop(auxiliary);
        }
        for id in 1..=count {
            let active = wait_for_active(server, std::time::Duration::from_secs(2));
            let byte = u8::try_from(id).unwrap();
            assert_eq!(active.payload(), &[byte]);
            active.send_response(&[byte + 10]).unwrap();
            let pending = &owned.pending[id - 1];
            assert_eq!(pending.connection_state(), StrictChannelStatus::Closed);
            assert_eq!(pending.try_recv().unwrap(), Some(vec![byte + 10]));
            assert_eq!(pending.try_recv().unwrap(), None);
        }
        assert!(server.try_recv().unwrap().is_none());
        owned.pending.clear();
        owned.client.inner.maintain();
        assert_eq!(owned.client.inner.number_of_active_requests(), 0);
        assert_eq!(owned.client.inner.number_of_detached_requests(), 0);
        // 下一次真实 loan 执行原 returned-samples 回收；不让 snapshot 代替维护。
        let original_loan = owned.client.inner.loan_slice_uninit(1).unwrap();
        drop(original_loan);
        let final_snapshot = owned.client.request_queue_fixture_snapshot().unwrap();
        assert_eq!(
            (
                final_snapshot.loans,
                final_snapshot.active,
                final_snapshot.detached,
                final_snapshot.pool_free
            ),
            (1, 0, 0, 9)
        );
        assert_eq!(
            final_snapshot.auxiliary_references,
            if mode == QueueFailure::NotEnqueued {
                1
            } else {
                2
            }
        );
        assert!(!final_snapshot.auxiliary_used);
        eprintln!(
            "原队列故障 {mode:?}：初态={initial:?} B结果={sent:?} 原server退出前={final_snapshot:?}"
        );
        drop(owned);
        eprintln!(
            "原队列故障 {mode:?}：原server、Pending、A本地loan、client和Node已按序退出；未知远端引用未当业务退休"
        );
    }

    #[cfg(feature = "test-rr-fixtures")]
    #[test]
    fn real_sender_cleanup_keeps_original_unknown_pending_detached() {
        let node = NodeBuilder::new().create::<IxService>().unwrap();
        let (client, server) = explicit_pair(&node, &fresh_topic("original_unknown"));
        let pending = client.send_typed(&[42]).unwrap();
        assert!(pending.delivery_issue().is_none());
        let active = wait_for_active(&server, std::time::Duration::from_secs(2));
        let record = active.fixture_abandon_response_sender().unwrap();
        assert!(record.removed);
        assert_eq!(record.client_id, client.inner.id().value());
        assert_eq!(pending.connection_state(), StrictChannelStatus::Unknown);
        assert!(!matches!(pending.try_recv(), Ok(Some(_))));
        assert_eq!(client.inner.number_of_active_requests(), 1);
        assert_eq!(client.inner.number_of_pinned_routes(), 1);
        drop(pending);
        client.inner.maintain();
        assert_eq!(client.inner.number_of_active_requests(), 1);
        assert_eq!(client.inner.number_of_detached_requests(), 1);
        assert_eq!(client.inner.number_of_pinned_routes(), 1);
        drop(active);
        client.inner.maintain();
        assert_eq!(client.inner.number_of_detached_requests(), 1);
        eprintln!(
            "原sender真实清理={record:?}；Unknown原请求仍占active/Detached/pin各1，非正常退休"
        );
        drop(server);
        drop(client);
        drop(node);
    }

    fn wait_for_active<Req: BodyMarker, Resp: BodyMarker>(
        server: &Iceoryx2RrServer<Req, Resp>,
        max: std::time::Duration,
    ) -> Iceoryx2ActiveRequest<Req, Resp> {
        let start = std::time::Instant::now();
        loop {
            if let Some(a) = server.try_recv().unwrap() {
                return a;
            }
            assert!(
                start.elapsed() < max,
                "server did not receive active request within {max:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    fn wait_for_response<Resp: BodyMarker>(
        pending: &Iceoryx2PendingResponse<Resp>,
        max: std::time::Duration,
    ) -> Vec<u8> {
        let start = std::time::Instant::now();
        loop {
            if let Some(b) = pending.try_recv().unwrap() {
                return b;
            }
            assert!(
                start.elapsed() < max,
                "client did not receive response within {max:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
}
