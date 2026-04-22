//! # robot-transport
//!
//! 本机通信层。`docs/00_design.md` §2 / §4.3 的 Rust 落地。
//!
//! Step 3（当前）：[`parser`] 模块 —— 按 §2.2 的 9 步验证顺序解析 iceoryx2
//! sample 为 [`parser::ParsedSample`]；[`metrics`] 模块 —— 每种 `ParseError`
//! 分类计数，供 subscriber 累积 metric + 触发升级 DEGRADED 的判定。
//!
//! Step 4 起会追加：`SampleLease<T>` / `SampleWriter<T>` / `LatestSubscriber<T>` /
//! `Publisher<T>` / `RequestClient<Req, Resp>` trait + mock 内存后端；
//! Step 5 再叠真 iceoryx2 后端。
//!
//! ## RT 约束
//!
//! - `parser::parse_sample` 零 alloc、零 panic、零 unwrap。
//! - RT 路径禁止构造 `FlatBufferBuilder`（§0 RT 禁令；publishers 在 RT 外
//!   准备 sample）。

#![forbid(unused_must_use)]

pub mod metrics;
pub mod parser;

pub use metrics::ParseMetrics;
pub use parser::{ParseError, ParsedSample, parse_sample};
