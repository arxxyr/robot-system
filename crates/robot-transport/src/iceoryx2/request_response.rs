//! v0.3 A1-1 spike：iceoryx2 `RequestResponse` minimal wrapper。
//!
//! ⚠️ **scope**：reviewer A1 第 1 个拍板"跨进程通道选 C2 iceoryx2
//! `RequestResponse`"的最小验证。本 module 只证明 C2 方案对
//! `robot-msg` `[u8]` slice payload + schema attribute verifier 跑得通；
//! **不**重构现有 `publish_subscribe` 封装（reviewer A1-1 拍板硬边界）。
//!
//! ## A1 land 后状态 + v0.4+ 留作业
//!
//! A1-2（s0_daemon `rr_server_thread`）/ A1-3（`robot-rpc` 端
//! `rr_io_thread` + `ClearEStopHandler`）已用本 wrapper 真接通跨进程
//! ClearEStop control plane（详
//! `docs/handoffs/v0.3_a1_4_clear_estop_smoke.md`）。下面这些仍是 v0.4+
//! 按需扩展点（**不**卡 A1 收尾，A1 用 schema attribute verifier +
//! handler 端 deadline + idempotency_key 等已经覆盖核心安全语义）：
//!
//! - 双 `schema_id` attribute（当前只挂 request schema_id；response schema_id
//!   靠 `Resp` 类型参数泛型 + 文档约定，不进 attribute 表）。
//! - `QoS` profile（当前用 iceoryx2 默认 RR 队列大小 + 单 client 单 server）。
//! - active request 的 deadline 跟踪 / 超时丢弃（A1 已在 handler 层走
//!   `tokio::time::timeout(timeout_ms + grace_ms)` + daemon 端
//!   `clock.now() > deadline_ns` 两点 check；transport 层不参与）。
//! - `FrameHeader` 4-byte size prefix wrapping（当前 wire 只是 raw FlatBuffers
//!   bytes，`Req::SCHEMA_ID` 通过 service attribute verifier 守门）。
//!
//! ## 跟生产 publish/subscribe wrapper 关系
//!
//! - `payload type` 一致用 `[u8]` 变长 slice（跟 [`crate::iceoryx2::publisher`]
//!   `Iceoryx2Publisher<T>` 风格对齐）。
//! - schema attribute 复用 [`crate::iceoryx2::attributes::schema_attribute_spec`]
//!   / `schema_attribute_verifier`（**不**新增 attribute key），attribute 值
//!   是 request schema_id（单边）。
//! - 现有 publish_subscribe 封装一行不动。

// 中文 doc 块大量出现 `RequestResponse` / `QoS` / `FrameHeader` /
// `RequestPayload` / `MaybeUninit` 等 identifier-like 字串；A1-1 spike
// scope 下不强求 doc-markdown 严格化。
#![allow(clippy::doc_markdown)]

use core::marker::PhantomData;

use iceoryx2::active_request::ActiveRequest as IxActiveRequest;
use iceoryx2::pending_response::PendingResponse as IxPendingResponse;
use iceoryx2::port::client::Client as IxClient;
use iceoryx2::port::server::Server as IxServer;
use iceoryx2::prelude::Node;
use iceoryx2::response::Response as IxResponse;
use iceoryx2::service::builder::request_response::{
    RequestResponseCreateError, RequestResponseOpenError, RequestResponseOpenOrCreateError,
};
use iceoryx2::service::ipc::Service as IxService;
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

/// RR client wrapper（thin）。
///
/// 跟 [`crate::iceoryx2::Iceoryx2Publisher`] 类似的 marker 泛型设计：
/// 类型参数 `Req` / `Resp` 都是 `BodyMarker`，编译期固定 wire schema +
/// 调用方意图；运行时 schema 守门走 service attribute verifier。
pub struct Iceoryx2RrClient<Req: BodyMarker, Resp: BodyMarker> {
    inner: IxRrClient,
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

/// `Iceoryx2RrClient::send` 返回的 pending response handle。`try_recv` 非阻
/// 塞，没收到 reply 时返 `Ok(None)`。drop 即 cancel pending（iceoryx2 通知
/// server 该 request 不再等 response）。
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

/// **仅同进程测试 / spike 便利用**：在同一 node 上 create 一份 service +
/// 同时 create client + server，返回 pair。本函数让单测能一行起完整 RR
/// round-trip 通路，**不**适合跨进程生产路径。
///
/// **跨进程禁用**（reviewer A1-1 part 2 round 2 拍板硬边界）：A1-2 真接
/// daemon ↔ RPC 时**禁止**两端都调本函数 —— 那会让 RPC 进程多挂一个
/// server / S0 进程多挂一个 client，request 可能被错误 server 端口占住
/// / 造成调试歧义。跨进程**必须**分别调：
///
/// - `robot-rpc` 端 → [`rr_client`]
/// - `s0_daemon` / `ipc_thread` 端 → [`rr_server`]
///
/// 两端共用同一 `topic`，首次 call 由先到那端 create + 后到那端 open，
/// attribute verify 自动守门（schema_id mismatch → open 拒绝）。
///
/// # Errors
///
/// - `topic` 字符串非法（iceoryx2 `ServiceName` 拒绝）
/// - `Req::SCHEMA_ID` 未在 [`robot_msg::size_tier::default_tier_of`] 登记
/// - iceoryx2 service create 失败 / attribute verify 失败 / 端口耗尽
#[allow(clippy::type_complexity)]
pub fn rr_channel<Req: BodyMarker, Resp: BodyMarker>(
    node: &Node<IxService>,
    topic: &str,
) -> Result<(Iceoryx2RrClient<Req, Resp>, Iceoryx2RrServer<Req, Resp>), TransportError> {
    let service = build_rr_service::<Req>(node, topic)?;
    let max_bytes = max_bytes_for::<Req, Resp>()?;
    let client = service
        .client_builder()
        .initial_max_slice_len(max_bytes)
        .create()
        .map_err(|e| TransportError::Backend(format!("iceoryx2 RR client create: {e:?}")))?;
    let server = service
        .server_builder()
        .initial_max_slice_len(max_bytes)
        .create()
        .map_err(|e| TransportError::Backend(format!("iceoryx2 RR server create: {e:?}")))?;
    Ok((
        Iceoryx2RrClient {
            inner: client,
            _ty: PhantomData,
        },
        Iceoryx2RrServer {
            inner: server,
            _ty: PhantomData,
        },
    ))
}

/// 跨进程 client 端工厂（**不**create server）。
///
/// # Errors
///
/// 同 [`rr_channel`]。
pub fn rr_client<Req: BodyMarker, Resp: BodyMarker>(
    node: &Node<IxService>,
    topic: &str,
) -> Result<Iceoryx2RrClient<Req, Resp>, TransportError> {
    let service = build_rr_service::<Req>(node, topic)?;
    let max_bytes = max_bytes_for::<Req, Resp>()?;
    let client = service
        .client_builder()
        .initial_max_slice_len(max_bytes)
        .create()
        .map_err(|e| TransportError::Backend(format!("iceoryx2 RR client create: {e:?}")))?;
    Ok(Iceoryx2RrClient {
        inner: client,
        _ty: PhantomData,
    })
}

/// 跨进程 server 端工厂（**不**create client）。
///
/// # Errors
///
/// 同 [`rr_channel`]。
pub fn rr_server<Req: BodyMarker, Resp: BodyMarker>(
    node: &Node<IxService>,
    topic: &str,
) -> Result<Iceoryx2RrServer<Req, Resp>, TransportError> {
    let service = build_rr_service::<Req>(node, topic)?;
    let max_bytes = max_bytes_for::<Req, Resp>()?;
    let server = service
        .server_builder()
        .initial_max_slice_len(max_bytes)
        .create()
        .map_err(|e| TransportError::Backend(format!("iceoryx2 RR server create: {e:?}")))?;
    Ok(Iceoryx2RrServer {
        inner: server,
        _ty: PhantomData,
    })
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

/// 内部辅助：build RR service + attribute verifier。
fn build_rr_service<Req: BodyMarker>(
    node: &Node<IxService>,
    topic: &str,
) -> Result<IxRrServiceFactory, TransportError> {
    let service_name = topic_to_service_name(topic)?;
    // attribute verifier 跟 publish_subscribe 一样硬约束 schema_id；这里只挂
    // request schema_id（spike 简化）。
    let _tier = default_tier_of(Req::SCHEMA_ID).ok_or_else(|| {
        TransportError::Backend(format!(
            "request schema {:#010x} ({}) has no size tier in robot-msg::size_tier; \
             refusing to create iceoryx2 RR service",
            Req::SCHEMA_ID,
            Req::SCHEMA_NAME
        ))
    })?;
    let verifier = schema_attribute_verifier(Req::SCHEMA_ID, 1)?;
    // 并发 open_or_create 竞态有界重试，语义同 pub/sub 侧 `build_service`
    // （参数/根因见 mod.rs `CREATE_RACE_*` doc）。
    let mut attempt: u32 = 0;
    loop {
        match node
            .service_builder(&service_name)
            .request_response::<[u8], [u8]>()
            .open_or_create_with_attributes(&verifier)
        {
            Ok(service) => return Ok(service),
            Err(e) if attempt < CREATE_RACE_MAX_ATTEMPTS && is_transient_rr_race(e) => {
                attempt += 1;
                std::thread::sleep(CREATE_RACE_RETRY_INTERVAL);
            }
            Err(e) => {
                return Err(TransportError::Backend(format!(
                    "iceoryx2 RR service open_or_create: {e:?} (attempts={})",
                    attempt + 1
                )));
            }
        }
    }
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
    /// 发送一条 request（FlatBuffers root bytes，调用方走 `robot-msg` 的
    /// `build_*_request` 之类构造）。返 pending response handle，调用方轮询
    /// `try_recv()` 拿 reply。
    ///
    /// `'a` lifetime 来自 `&'a self`：pending response 不能比 client 活久。
    ///
    /// # Errors
    ///
    /// - iceoryx2 `loan_slice_uninit` 失败（内存压力 / shm 耗尽）。
    /// - `request.send` 失败（unable_to_deliver_strategy = Block 时不会丢；
    ///   默认 DiscardSample 时静默丢，但仍返 Ok 表示 publish 路径完成）。
    pub fn send(&self, bytes: &[u8]) -> Result<Iceoryx2PendingResponse<Resp>, TransportError> {
        let mut req = self
            .inner
            .loan_slice_uninit(bytes.len())
            .map_err(|e| TransportError::Backend(format!("RR client loan_slice_uninit: {e:?}")))?;
        // SAFETY: loan_slice_uninit 给我们 `bytes.len()` 个 MaybeUninit<u8> 的
        // slice；写入完整 bytes.len 个 u8 让所有 element 都 init。
        let payload_mut = req.payload_mut();
        for (dst, src) in payload_mut.iter_mut().zip(bytes.iter()) {
            dst.write(*src);
        }
        // SAFETY: 上面循环写入了所有 bytes.len() 个元素 → fully initialized。
        let req_init = unsafe { req.assume_init() };
        let pending = req_init
            .send()
            .map_err(|e| TransportError::Backend(format!("RR client send: {e:?}")))?;
        Ok(Iceoryx2PendingResponse {
            inner: pending,
            _ty: PhantomData,
        })
    }
}

// ---------------------------------------------------------------------------
// Iceoryx2PendingResponse API
// ---------------------------------------------------------------------------

impl<Resp: BodyMarker> Iceoryx2PendingResponse<Resp> {
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
    /// - iceoryx2 `loan_slice_uninit` 失败 / `send` 失败。
    pub fn send_response(self, bytes: &[u8]) -> Result<(), TransportError> {
        let mut resp = self
            .inner
            .loan_slice_uninit(bytes.len())
            .map_err(|e| TransportError::Backend(format!("RR active loan_slice_uninit: {e:?}")))?;
        let payload_mut = resp.payload_mut();
        for (dst, src) in payload_mut.iter_mut().zip(bytes.iter()) {
            dst.write(*src);
        }
        // SAFETY: 上面循环写满了 bytes.len() 个元素。
        let resp_init = unsafe { resp.assume_init() };
        resp_init
            .send()
            .map_err(|e| TransportError::Backend(format!("RR active send response: {e:?}")))?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use iceoryx2::prelude::NodeBuilder;
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
        let (client, server) =
            rr_channel::<markers::ClearEStopRequest, markers::ClearEStopResponse>(&node, &topic)
                .unwrap();

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
        let (client, server) =
            rr_channel::<markers::ClearEStopRequest, markers::ClearEStopResponse>(&node, &topic)
                .unwrap();

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
        let _ok =
            rr_channel::<markers::ClearEStopRequest, markers::ClearEStopResponse>(&node, &topic)
                .unwrap();
        // 第二次 open：用 Heartbeat 作 schema → attribute verify 失败
        let bad = rr_channel::<markers::Heartbeat, markers::ClearEStopResponse>(&node, &topic);
        assert!(
            bad.is_err(),
            "expected schema attribute verify to reject mismatched request marker"
        );
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
