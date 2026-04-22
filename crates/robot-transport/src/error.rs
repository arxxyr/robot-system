//! `TransportError` —— `robot-transport` 对外的统一错误分类。
//!
//! 覆盖三类来源：
//!
//! 1. sample 解析（[`crate::parser::ParseError`]，`From` 自动提升为
//!    [`TransportError::Parse`]）
//! 2. 后端资源（loan 超 service 大小上限 / publisher 池耗尽 / subscriber
//!    空队列 / 下层 iceoryx2 / mock 异常）
//! 3. 跨越 `SampleLease::view::<T>()` 的 typed 校验失败
//!    （`schema_id` 与 marker 不匹配、blob alignment 不满足）

use crate::parser::ParseError;
use thiserror::Error;

/// Sample 处理中遇到的所有错误分类。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TransportError {
    /// 底层 [`crate::parser::parse_sample`] 验证失败。所有 9 步 header /
    /// body / blob 边界错误由此传播。
    #[error("sample parse failed: {0}")]
    Parse(#[from] ParseError),

    /// `SampleLease::view::<T>()` 时发现 sample 的 `FrameHeader.schema_id`
    /// 与 marker `T::SCHEMA_ID` 不匹配（订阅了错 topic / 发布方路由错）。
    #[error(
        "schema_id mismatch: sample carries {actual:#010x} ({actual_name}), \
         but T expects {expected:#010x} ({expected_name})"
    )]
    SchemaMismatch {
        expected: u32,
        expected_name: &'static str,
        actual: u32,
        actual_name: &'static str,
    },

    /// typed body 反序列化失败（body bytes 自身合法 `FlatBuffers`，但无法按
    /// marker 对应的 root table verify）。
    #[error("typed body verify failed for {marker}: {cause}")]
    BodyVerifyFailed { marker: &'static str, cause: String },

    /// `blob_offset` 未满足 `BlobDesc.alignment`（§2.2 第 7 步的 alignment
    /// 部分，Step 4 真实施）。
    #[error("blob misaligned: blob_offset={blob_offset}, required alignment={alignment}")]
    BlobMisaligned { blob_offset: u32, alignment: u16 },

    /// 发布方 `loan()` 请求的 sample 超过 service 声明的 `max_sample_size`。
    #[error("sample size {size} exceeds service limit {limit}")]
    SampleTooLarge { size: usize, limit: usize },

    /// 订阅方队列为空（latest-sample slot 未被写过）。
    #[error("subscriber has no sample available")]
    Empty,

    /// 发布方所有预分配 sample 都在使用中（RT 路径的 backpressure）。
    #[error("publisher has no free sample slot")]
    NoFreeSample,

    /// 下层后端未分类错误 —— iceoryx2 原生错误 / mock 测试辅助抛出 /
    /// 系统 syscall 异常。字符串字段只用于诊断，不参与匹配。
    #[error("transport backend error: {0}")]
    Backend(String),

    /// [`crate::traits::RequestClient::call`] 的 deadline 过了还没收到 response。
    #[error("request-response deadline exceeded")]
    DeadlineExceeded,
}
