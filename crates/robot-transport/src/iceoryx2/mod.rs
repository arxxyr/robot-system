//! `iceoryx2` 后端（§2.1）。Step 5 分阶段上：
//!
//! - **本 commit**：[`service_name`] + [`qos`] 纯类型层，不含真实 pub/sub。
//! - **后续 commit**：`attributes` / `publisher` / `subscriber` / `event`
//!   wrapper。
//!
//! ### 设计决策
//!
//! 决策冻结在 `docs/CHANGELOG.md` 2026-04-22 "Step 5 开工决策与 iceoryx2
//! 依赖登记"：
//!
//! 1. 版本精确 pin `iceoryx2 = "=0.8.1"`（升级必重跑 `/tmp` 烟测）。
//! 2. topic 字符串原样当 `ServiceName`（方案 A），§2.3 命名空间保持不变。
//! 3. `QoS` 按 §2.4 九类映射到 iceoryx2 builder 选项（见 [`qos`]）。
//! 4. sample 上限按 [`robot_msg::size_tier::SizeTier`] 四档。
//! 5. `schema_id` / `schema_version` 作为 service attribute 双道防线
//!    （attribute 是硬约束，mismatch open 被拒绝）。
//!
//! ### RT 约束
//!
//! - service / publisher / subscriber 构造**不得进 RT thread**（涉及 shm 申请
//!   / 锁 / 文件系统）。构造必须在 `s0_ipc_thread` 启动期 / `s1_coordinator`
//!   non-RT 路径上一次性完成。
//! - 运行期 `publisher.loan_uninit` / `subscriber.receive` 零拷贝零 alloc，
//!   RT thread 可直接调（但按 §1.2 S0 RT thread 仍不直接碰 iceoryx2，
//!   走 `s0_ipc_thread` 代理）。

pub mod attributes;
pub mod publisher;
pub mod qos;
pub mod service_name;
pub mod subscriber;

pub use attributes::{
    ATTR_SCHEMA_ID, ATTR_SCHEMA_VERSION, schema_attribute_spec, schema_attribute_verifier,
};
pub use publisher::{Iceoryx2Publisher, Iceoryx2SampleWriter};
pub use qos::{QosProfile, TopicClass, profile_of};
pub use service_name::topic_to_service_name;
pub use subscriber::{Iceoryx2SampleLease, Iceoryx2Subscriber};

// ---------------------------------------------------------------------------
// Service open-or-create 入口
// ---------------------------------------------------------------------------
//
// 对调用方（robot-s0 / robot-s1 / robot-tests）暴露单入口：
//   let (pub_, sub) = iceoryx2::channel::<JointState>(&node, topic, TopicClass::S0State)?;
// 内部按决策 ①②③④⑤ 一起落：
//   - service name: topic 原样
//   - QoS profile: profile_of(class)
//   - size tier: default_tier_of(T::SCHEMA_ID) → max_sample_size
//   - attribute: schema_id + schema_version 硬绑
//   - publisher / subscriber 同时创建（同进程 pub+sub）
// 跨进程场景（真正的 s0/s1/viz 多进程）下，open_or_create 的发布端 / 订阅
// 端各走一半；本 MVP 给一个对称 helper 方便单测。

use core::marker::PhantomData;

use iceoryx2::prelude::Node;
use iceoryx2::service::ipc::Service as IxService;
use robot_msg::markers::BodyMarker;
use robot_msg::size_tier::default_tier_of;

use crate::error::TransportError;

/// 同进程对称 pub/sub 创建：返回已配置好 service attributes / `QoS` / tier
/// 的一对发布者 + 订阅者。生产路径（跨进程）下分别调用各自半边即可。
///
/// # Errors
///
/// - `topic` 字符串非法（iceoryx2 `ServiceName::new` 拒绝）
/// - `T::SCHEMA_ID` 未在 [`robot_msg::size_tier::default_tier_of`] 登记（未知
///   schema 拒绝创建 service）
/// - iceoryx2 service 创建失败（权限 / 端口耗尽 / 后端异常）
pub fn channel<T: BodyMarker>(
    node: &Node<IxService>,
    topic: &str,
    class: TopicClass,
) -> Result<(Iceoryx2Publisher<T>, Iceoryx2Subscriber<T>), TransportError> {
    let service_name = topic_to_service_name(topic)?;
    let tier = default_tier_of(T::SCHEMA_ID).ok_or_else(|| {
        TransportError::Backend(format!(
            "schema {:#010x} ({}) has no size tier in robot-msg::size_tier; \
             refusing to create iceoryx2 service",
            T::SCHEMA_ID,
            T::SCHEMA_NAME
        ))
    })?;
    let max_bytes = tier.max_bytes();
    let profile = profile_of(class);
    let spec = schema_attribute_spec(T::SCHEMA_ID, 1)?;

    let service = node
        .service_builder(&service_name)
        .publish_subscribe::<[u8]>()
        .history_size(profile.history_size)
        .subscriber_max_buffer_size(profile.subscriber_max_buffer_size)
        .max_publishers(profile.max_publishers)
        .max_subscribers(profile.max_subscribers)
        .enable_safe_overflow(profile.safe_overflow)
        .create_with_attributes(&spec)
        .map_err(|e| TransportError::Backend(format!("iceoryx2 service create: {e:?}")))?;

    let publisher = service
        .publisher_builder()
        .initial_max_slice_len(max_bytes)
        .create()
        .map_err(|e| TransportError::Backend(format!("iceoryx2 publisher create: {e:?}")))?;

    let subscriber = service
        .subscriber_builder()
        .create()
        .map_err(|e| TransportError::Backend(format!("iceoryx2 subscriber create: {e:?}")))?;

    Ok((
        Iceoryx2Publisher {
            inner: publisher,
            loan_tok: 0,
            _ty: PhantomData,
        },
        Iceoryx2Subscriber {
            inner: subscriber,
            _ty: PhantomData,
        },
    ))
}
