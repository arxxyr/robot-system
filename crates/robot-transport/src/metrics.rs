//! `ParseError` 分类计数器。
//!
//! 对应 `docs/00_design.md` §8《观测与调试》里 "metric hook：每种错误计数"
//! 的要求。每个 subscriber（`s0_ipc_thread` / `mock_s1` / `robotctl` 等）持
//! 一份 `ParseMetrics`，parse 失败时调用 `record(&err)`；监控侧按 topic +
//! 错误类型聚合，支撑 §10.1 `command topic drop = 0` / §3.5.1
//! `unknown_layout_count = 0` 等硬约束。

use core::sync::atomic::{AtomicU64, Ordering};

use crate::parser::ParseError;

/// 每种 `ParseError` 变体对应一个 `AtomicU64` 计数。字段 pub 方便调用方直接
/// `.load(Ordering::Relaxed)` 拿值；`record` 按变体自动累加。
#[derive(Debug, Default)]
pub struct ParseMetrics {
    /// 对应 [`ParseError::SizeTooSmall`]。
    pub size_too_small: AtomicU64,
    /// 对应 [`ParseError::HeaderSizeInvalid`]。
    pub header_size_invalid: AtomicU64,
    /// 对应 [`ParseError::HeaderVerifyFailed`]。
    pub header_verify_failed: AtomicU64,
    /// 对应 [`ParseError::BodyOutOfBounds`]。
    pub body_out_of_bounds: AtomicU64,
    /// 对应 [`ParseError::BlobOutOfBounds`]。
    pub blob_out_of_bounds: AtomicU64,
    /// 对应 [`ParseError::BlobOverlap`]。
    pub blob_overlap: AtomicU64,
    /// 对应 [`ParseError::UnknownSchemaId`]。
    pub unknown_schema_id: AtomicU64,
}

/// `snapshot()` 返回的值语义快照，方便一次性导出到 Prometheus / log。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ParseMetricsSnapshot {
    pub size_too_small: u64,
    pub header_size_invalid: u64,
    pub header_verify_failed: u64,
    pub body_out_of_bounds: u64,
    pub blob_out_of_bounds: u64,
    pub blob_overlap: u64,
    pub unknown_schema_id: u64,
}

impl ParseMetricsSnapshot {
    /// 全部类别的错误总和。
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.size_too_small
            + self.header_size_invalid
            + self.header_verify_failed
            + self.body_out_of_bounds
            + self.blob_out_of_bounds
            + self.blob_overlap
            + self.unknown_schema_id
    }
}

impl ParseMetrics {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            size_too_small: AtomicU64::new(0),
            header_size_invalid: AtomicU64::new(0),
            header_verify_failed: AtomicU64::new(0),
            body_out_of_bounds: AtomicU64::new(0),
            blob_out_of_bounds: AtomicU64::new(0),
            blob_overlap: AtomicU64::new(0),
            unknown_schema_id: AtomicU64::new(0),
        }
    }

    /// 按 `err` 的变体累加对应计数（`Ordering::Relaxed` —— 纯统计）。
    pub fn record(&self, err: &ParseError) {
        let counter = match err {
            ParseError::SizeTooSmall { .. } => &self.size_too_small,
            ParseError::HeaderSizeInvalid { .. } => &self.header_size_invalid,
            ParseError::HeaderVerifyFailed { .. } => &self.header_verify_failed,
            ParseError::BodyOutOfBounds { .. } => &self.body_out_of_bounds,
            ParseError::BlobOutOfBounds { .. } => &self.blob_out_of_bounds,
            ParseError::BlobOverlap { .. } => &self.blob_overlap,
            ParseError::UnknownSchemaId { .. } => &self.unknown_schema_id,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// 一次性读出所有计数；读序 `Relaxed`（快照可能处于 "不同类目读时间
    /// 略有错位" 的状态，单纯统计可接受）。
    #[must_use]
    pub fn snapshot(&self) -> ParseMetricsSnapshot {
        ParseMetricsSnapshot {
            size_too_small: self.size_too_small.load(Ordering::Relaxed),
            header_size_invalid: self.header_size_invalid.load(Ordering::Relaxed),
            header_verify_failed: self.header_verify_failed.load(Ordering::Relaxed),
            body_out_of_bounds: self.body_out_of_bounds.load(Ordering::Relaxed),
            blob_out_of_bounds: self.blob_out_of_bounds.load(Ordering::Relaxed),
            blob_overlap: self.blob_overlap.load(Ordering::Relaxed),
            unknown_schema_id: self.unknown_schema_id.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::FlatbufferFailureKind;

    #[test]
    fn record_each_variant_increments_its_counter() {
        let m = ParseMetrics::new();
        m.record(&ParseError::SizeTooSmall { size: 2 });
        m.record(&ParseError::SizeTooSmall { size: 3 });
        m.record(&ParseError::HeaderSizeInvalid {
            h: 0,
            sample_size: 4,
            max: 4096,
        });
        m.record(&ParseError::HeaderVerifyFailed {
            reason: FlatbufferFailureKind::InvalidStructure,
        });
        m.record(&ParseError::BodyOutOfBounds {
            body_offset: 10,
            body_size: 100,
            header_end: 5,
            sample_size: 50,
        });
        m.record(&ParseError::BlobOutOfBounds {
            blob_offset: 10,
            blob_size: 100,
            sample_size: 50,
        });
        m.record(&ParseError::BlobOverlap {
            body_start: 10,
            body_end: 30,
            blob_start: 20,
            blob_end: 40,
        });
        m.record(&ParseError::UnknownSchemaId {
            schema_id: 0xDEAD_BEEF,
        });

        let s = m.snapshot();
        assert_eq!(s.size_too_small, 2);
        assert_eq!(s.header_size_invalid, 1);
        assert_eq!(s.header_verify_failed, 1);
        assert_eq!(s.body_out_of_bounds, 1);
        assert_eq!(s.blob_out_of_bounds, 1);
        assert_eq!(s.blob_overlap, 1);
        assert_eq!(s.unknown_schema_id, 1);
        assert_eq!(s.total(), 8);
    }

    #[test]
    fn default_snapshot_is_zero() {
        assert_eq!(
            ParseMetrics::new().snapshot(),
            ParseMetricsSnapshot::default()
        );
    }
}
