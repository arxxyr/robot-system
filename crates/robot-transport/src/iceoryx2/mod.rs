//! `iceoryx2` IPC 后端（§2.1）：pub/sub、强类型 request/response 与只读观测。
//!
//! 版本由 workspace 与 vendor 源码统一锁定，当前为 `iceoryx2 = "=0.9.3"`。
//!
//! ### 服务合同
//!
//! - topic 原样作为 `ServiceName`，保留 §2.3 命名空间。
//! - pub/sub 的 `QoS` 由 [`qos`] 映射，sample 上限由
//!   [`robot_msg::size_tier::SizeTier`] 决定。
//! - `schema_id` / `schema_version` 绑定 service attribute，不匹配时拒绝打开。
//! - 订阅视图借用 sample lease，不得超出该 lease 的生命周期。
//!
//! ### RT 约束
//!
//! 服务、端口构造与运行期 IPC 均留在普通优先级线程，不进入 S0 RT 线程。
//! 构造涉及 shm、锁及文件系统；当前发布 wrapper 还会构造缓冲并复制到 shm，
//! 不能据底层 loan API 宣称本封装全链零拷贝或零分配。

pub mod attributes;
pub mod observer;
pub mod publisher;
pub mod qos;
pub mod request_response;
pub mod service_name;
pub mod subscriber;

pub use attributes::{
    ATTR_SCHEMA_ID, ATTR_SCHEMA_VERSION, schema_attribute_spec, schema_attribute_verifier,
};
pub use observer::{
    ObserverDiscovery, ObserverMetadata, ObserverMonitor, ObserverSample, ObserverStats,
    ObserverSubscriber, ObserverTopic, list_observers, list_observers_filtered,
    observer_subscriber, observer_topic,
};
pub use publisher::{Iceoryx2Publisher, Iceoryx2SampleWriter};
pub use qos::{QosProfile, TopicClass, profile_of};
pub use request_response::{
    EnqueueIssue, LoanKind, NotDelivered, RrPeerId, RrProfile, RrSendError, RrStaticLimits,
    StrictChannelStatus, rr_client_with, rr_server_with,
};
#[cfg(feature = "test-rr-fixtures")]
#[doc(hidden)]
pub use request_response::{
    FixtureError, QueueFailure, QueueFixtureSnapshot, REQUEST_QUEUE_FIXTURE_AUXILIARY,
    SenderCleanupRecord,
};
pub use service_name::topic_to_service_name;
pub use subscriber::{Iceoryx2SampleLease, Iceoryx2Subscriber};

// ---------------------------------------------------------------------------
// Service open-or-create 入口
// ---------------------------------------------------------------------------
//
// channel 创建同进程 pub/sub 对；生产调用方也可分别创建 publisher / subscriber：
//   let (pub_, sub) = iceoryx2::channel::<JointState>(&node, topic, TopicClass::S0State)?;
// 各入口共用服务合同：
//   - service name: topic 原样
//   - QoS profile: profile_of(class)
//   - size tier: default_tier_of(T::SCHEMA_ID) → max_sample_size
//   - attribute: schema_id + schema_version 硬绑
//   - publisher / subscriber 同时创建（同进程 pub+sub）
// 跨进程各端按需持有对应端口，open_or_create 复用同名服务并检查 schema。

use core::marker::PhantomData;
use std::time::Duration;

use iceoryx2::prelude::Node;
use iceoryx2::service::builder::publish_subscribe::{
    PublishSubscribeCreateError, PublishSubscribeOpenError, PublishSubscribeOpenOrCreateError,
};
use iceoryx2::service::ipc::Service as IxService;
use iceoryx2::service::port_factory::publish_subscribe::PortFactory;
use robot_msg::markers::BodyMarker;
use robot_msg::size_tier::default_tier_of;

use crate::error::TransportError;

/// 并发 `open_or_create` 竞态重试参数（pub/sub 与 RR 共用）。
///
/// 两进程同时首建同一 service 时（s0 / s1 / gateway 并发 boot），
/// `open_or_create` 输家会**瞬时**报 `ServiceInCorruptedState` /
/// `HangsInCreation` / `AlreadyExists` / `SystemInFlux` 等瞬态错——赢家的
/// create 尚未完成，输家看到半成品。iceoryx2 0.8.1 与 0.9.3 实测同在
/// （`tests/iox2_sub_first_repro.rs` 并发变体 iter 0 即中）。有界重试跨过
/// 赢家的创建窗口；重试耗尽（真损坏 / 创建者真死）按原错误上抛。
///
/// 上限 = 10 ms × 50 = 500 ms，覆盖 daemon 启动期一次 service 创建的最坏
/// 情况；本路径**只在启动期**走（RT 约束见模块 doc），不影响运行期热路径。
pub(crate) const CREATE_RACE_RETRY_INTERVAL: Duration = Duration::from_millis(10);
pub(crate) const CREATE_RACE_MAX_ATTEMPTS: u32 = 50;

/// 判定 pub/sub `open_or_create` 错误是否为并发创建竞态的瞬态错（可重试）。
///
/// 穷举 match：上游加 variant 时编译期报警，重新分类而不是静默落 `false`。
fn is_transient_pubsub_race(e: PublishSubscribeOpenOrCreateError) -> bool {
    match e {
        // 上游文档明言：另一进程高频 create/remove 同名 service 时出现。
        PublishSubscribeOpenOrCreateError::SystemInFlux => true,
        PublishSubscribeOpenOrCreateError::PublishSubscribeOpenError(oe) => matches!(
            oe,
            // 存在性判定与 open 之间赢家半成品 / 已销毁的 TOCTOU 相。
            PublishSubscribeOpenError::DoesNotExist
                | PublishSubscribeOpenError::ServiceInCorruptedState
                | PublishSubscribeOpenError::HangsInCreation
                | PublishSubscribeOpenError::IsMarkedForDestruction
        ),
        PublishSubscribeOpenOrCreateError::PublishSubscribeCreateError(ce) => matches!(
            ce,
            // create 输家：service 刚被赢家建出 / 正在被建。
            PublishSubscribeCreateError::AlreadyExists
                | PublishSubscribeCreateError::IsBeingCreatedByAnotherInstance
                | PublishSubscribeCreateError::HangsInCreation
                | PublishSubscribeCreateError::ServiceInCorruptedState
        ),
    }
}

/// 同进程对称 pub/sub 创建：返回已配置好 service attributes / `QoS` / tier
/// 的一对发布者 + 订阅者。跨进程场景下也可以调用这个函数（每一端各拿自己要
/// 的那一半），因为底层走 `open_or_create_with_attributes` —— 首次调用 create，
/// 后续相同 topic 的调用自动 open 同一 service 并校验 schema attribute。
///
/// # Errors
///
/// - `topic` 字符串非法（iceoryx2 `ServiceName::new` 拒绝）
/// - `T::SCHEMA_ID` 未在 [`robot_msg::size_tier::default_tier_of`] 登记（未知
///   schema 拒绝创建 service）
/// - iceoryx2 service 创建失败（权限 / 端口耗尽 / 后端异常 / attribute 不匹配）
pub fn channel<T: BodyMarker>(
    node: &Node<IxService>,
    topic: &str,
    class: TopicClass,
) -> Result<(Iceoryx2Publisher<T>, Iceoryx2Subscriber<T>), TransportError> {
    let service = build_service::<T>(node, topic, class)?;
    let max_bytes = default_tier_of(T::SCHEMA_ID)
        .expect("default_tier_of is already validated by build_service")
        .max_bytes();

    let publisher = service
        .publisher_builder()
        .initial_max_slice_len(max_bytes)
        .create()
        .map_err(|e| TransportError::Backend(format!("iceoryx2 publisher create: {e:?}")))?;

    let subscriber = service
        .subscriber_builder()
        .create()
        .map_err(|e| TransportError::Backend(format!("iceoryx2 subscriber create: {e:?}")))?;

    let observer = observer::ObserverDispatcher::start(
        node,
        topic,
        T::SCHEMA_ID,
        profile_of(class).max_publishers,
        max_bytes,
        publisher.id().value().to_le_bytes(),
    );
    Ok((
        Iceoryx2Publisher {
            inner: publisher,
            observer,
            loan_tok: 0,
            _ty: PhantomData,
        },
        Iceoryx2Subscriber {
            inner: subscriber,
            _ty: PhantomData,
        },
    ))
}

/// 只 open/create subscriber —— 跨进程场景下"订阅端"专用（S0 subscribe S1 的
/// command/heartbeat，robotctl subscribe S0 的 state 等）。
///
/// 不创建 publisher；service 在首次调用时用 schema attribute 初始化，后续
/// 相同 topic 调用会走 open 路径。
///
/// # Errors
///
/// 同 [`channel`]。
pub fn subscriber<T: BodyMarker>(
    node: &Node<IxService>,
    topic: &str,
    class: TopicClass,
) -> Result<Iceoryx2Subscriber<T>, TransportError> {
    let service = build_service::<T>(node, topic, class)?;
    let sub = service
        .subscriber_builder()
        .create()
        .map_err(|e| TransportError::Backend(format!("iceoryx2 subscriber create: {e:?}")))?;
    Ok(Iceoryx2Subscriber {
        inner: sub,
        _ty: PhantomData,
    })
}

/// 只 open/create publisher —— 跨进程场景下"发布端"专用（`mock_s1` publish
/// command/heartbeat，S0 `pub_thread` publish state 等）。
///
/// # Errors
///
/// 同 [`channel`]。
pub fn publisher<T: BodyMarker>(
    node: &Node<IxService>,
    topic: &str,
    class: TopicClass,
) -> Result<Iceoryx2Publisher<T>, TransportError> {
    let service = build_service::<T>(node, topic, class)?;
    let max_bytes = default_tier_of(T::SCHEMA_ID)
        .expect("default_tier_of is already validated by build_service")
        .max_bytes();
    let pub_ = service
        .publisher_builder()
        .initial_max_slice_len(max_bytes)
        .create()
        .map_err(|e| TransportError::Backend(format!("iceoryx2 publisher create: {e:?}")))?;
    let observer = observer::ObserverDispatcher::start(
        node,
        topic,
        T::SCHEMA_ID,
        profile_of(class).max_publishers,
        max_bytes,
        pub_.id().value().to_le_bytes(),
    );
    Ok(Iceoryx2Publisher {
        inner: pub_,
        observer,
        loan_tok: 0,
        _ty: PhantomData,
    })
}

/// 内部辅助：`open_or_create_with_attributes` service + `QoS` 参数。
///
/// `schema_attribute_spec` 不再需要 —— verifier 的 required attributes 在
/// create 分支会被 iceoryx2 自动写入 service（§"决策 ⑤C 双道防线"）。多次
/// 调用相同 topic 时，create 只命中一次，后续走 open 并做 attribute verify。
fn build_service<T: BodyMarker>(
    node: &Node<IxService>,
    topic: &str,
    class: TopicClass,
) -> Result<PortFactory<IxService, [u8], ()>, TransportError> {
    let service_name = topic_to_service_name(topic)?;
    let tier = default_tier_of(T::SCHEMA_ID).ok_or_else(|| {
        TransportError::Backend(format!(
            "schema {:#010x} ({}) has no size tier in robot-msg::size_tier; \
             refusing to create iceoryx2 service",
            T::SCHEMA_ID,
            T::SCHEMA_NAME
        ))
    })?;
    let _ = tier; // 仅验证存在；max_bytes 在 publisher 侧取
    let profile = profile_of(class);
    let verifier = schema_attribute_verifier(T::SCHEMA_ID, 1)?;

    // builder 链每次 attempt 被消费，循环内重建（构造本身零成本）。
    let mut attempt: u32 = 0;
    loop {
        match node
            .service_builder(&service_name)
            .publish_subscribe::<[u8]>()
            .history_size(profile.history_size)
            .subscriber_max_buffer_size(profile.subscriber_max_buffer_size)
            .max_publishers(profile.max_publishers)
            .max_subscribers(profile.max_subscribers)
            .enable_safe_overflow(profile.safe_overflow)
            .open_or_create_with_attributes(&verifier)
        {
            Ok(service) => return Ok(service),
            Err(e) if attempt < CREATE_RACE_MAX_ATTEMPTS && is_transient_pubsub_race(e) => {
                attempt += 1;
                std::thread::sleep(CREATE_RACE_RETRY_INTERVAL);
            }
            Err(e) => {
                return Err(TransportError::Backend(format!(
                    "iceoryx2 service open_or_create: {e:?} (attempts={})",
                    attempt + 1
                )));
            }
        }
    }
}
