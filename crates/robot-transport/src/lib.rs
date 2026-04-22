//! # robot-transport
//!
//! 本机通信层。`docs/00_design.md` §2 / §4.3 的 Rust 落地。
//!
//! ### 模块
//!
//! - [`parser`]：按 §2.2 的 9 步验证顺序把 sample bytes 切成
//!   [`ParsedSample`]（header + body + optional blob），零 alloc。
//! - [`metrics`]：每种 [`ParseError`] 变体对应一个 `AtomicU64` 计数器。
//! - [`error`]：`TransportError` 顶层错误分类（聚合 `ParseError` + typed
//!   验证失败 + 后端错误）。
//! - [`sample`]：`SampleLease<T>` / `MessageView` / `BodyView` / `BlobView`。
//! - [`writer`]：`SampleWriter<'_, T>` 及其按值消费的 `publish(self)`。
//! - [`traits`]：`LatestSubscriber<T>` / `Publisher<T>` / `RequestClient<Req, Resp>`。
//! - [`mock`]（`feature = "mock"`，Step 4 默认开启）：内存后端，测试用
//!   `MockChannel` 驱动 publisher / subscriber 并观测 loan / publish /
//!   active-lease / active-writer 计数守恒。Step 5 引入 iceoryx2 feature 后
//!   default 会改为 iceoryx2，mock 作为 dev-dep 继续保留。
//!
//! ### RT 约束
//!
//! - `parser::parse_sample` 零 alloc、零 panic、零 unwrap。
//! - `SampleLease::view()` 借用 lease 内 bytes，RT 线程可调；但 `FlatBuffers`
//!   builder 构造仍然只允许非 RT 路径（§0 RT 禁令）。
//! - `SampleWriter` drop without publish = discard；`publish(self)` 按值消费。

#![forbid(unused_must_use)]

pub mod error;
pub mod metrics;
pub mod parser;
pub mod sample;
pub mod traits;
pub mod writer;

#[cfg(feature = "mock")]
pub mod mock;

pub use error::TransportError;
pub use metrics::ParseMetrics;
pub use parser::{ParseError, ParsedSample, parse_sample};
pub use sample::{BlobView, BodyView, HeaderView, MessageView, SampleLease};
pub use traits::{LatestSubscriber, Publisher, RequestClient};
pub use writer::SampleWriter;
