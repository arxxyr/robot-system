//! Sample 解析：按 `docs/00_design.md` §2.2 的 9 步验证顺序把一块 iceoryx2
//! sample 字节切成 `FrameHeader` + body + optional blob。
//!
//! ## 验证顺序（与 §2.2 逐条对应）
//!
//! 1. `sample_size >= 4`
//! 2. 读首 4B LE 得 `frame_header_size` (`H`)
//! 3. `0 < H <= 4096`（防御性上限）
//! 4. `flatbuffers::root::<FrameHeader>(&sample[4..4+H])`（`FlatBuffers` verifier）
//! 5. `body_offset >= 4 + H`
//! 6. `body_offset + body_size <= sample_size`
//! 7. 若 `HAS_BLOB`：`blob_offset + blob_size <= sample_size`
//!    （alignment 校验推迟到 Step 4 `SampleLease::view()`，彼时已知 `BlobDesc`）
//! 8. body 区间与 blob 区间不重叠
//! 9. 按 `schema_id` 查 `SCHEMA_REGISTRY`，未知即 `UnknownSchemaId`
//!
//! ## 合约
//!
//! - **不 panic、不 unwrap**：任何输入都走 `Result`。
//! - **零 alloc**：返回值借用输入 slice。
//! - RT 线程上本函数**可调用**；但注意 `FlatBufferBuilder` 构造不在此路径上。
//!
//! ## alignment 为什么此层不做
//!
//! `BlobDesc.alignment` 埋在 body 里（每个带 blob 的 schema 各自有 `blob:
//! BlobDesc` 字段）。本层不知道具体 schema 语义，无法统一访问。`SampleLease::
//! view::<T>()`（Step 4）落地 typed 访问时会补 alignment 检查并升级成
//! `BlobMisaligned` 错误。

use flatbuffers::InvalidFlatbuffer;
use robot_msg::capability::{CAP_HAS_BLOB, has};
use robot_msg::msg::FrameHeader;
use robot_msg::schema_id::lookup_name;
use thiserror::Error;

/// `FrameHeader` 长度上限（§2.2 第 3 步）。
pub const FRAME_HEADER_SIZE_MAX: u32 = 4096;

/// 解析成功后得到的 sample 视图。所有字段都是对输入 slice 的借用。
///
/// `header` 是 `FlatBuffers` verified `FrameHeader`，可以直接调 accessor
/// （`header.stamp_ns()` 等）；`body` 是字节切片，调用方按 `schema_id`
/// 进一步 `flatbuffers::root::<T>(body)` 即可解析 body table。
#[derive(Debug, Clone, Copy)]
pub struct ParsedSample<'a> {
    /// `FlatBuffers` verified `FrameHeader`。
    pub header: FrameHeader<'a>,
    /// body 原始字节，长度 = `header.body_size()`。
    pub body: &'a [u8],
    /// blob 原始字节（若 `capability_flags & CAP_HAS_BLOB != 0`，否则 `None`）。
    pub blob: Option<&'a [u8]>,
    /// 便利字段：`header.schema_id()`。
    pub schema_id: u32,
    /// 便利字段：`schema_id` 对应的规范名（注册表查表所得）。
    pub schema_name: &'static str,
}

/// 解析 / 验证错误分类。字段数字用于观测 / 诊断，所有变体 `Copy + Eq`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ParseError {
    /// sample 总长度不足 4 字节，连 `frame_header_size` 都读不到。
    #[error("sample size too small: {size} < 4")]
    SizeTooSmall { size: usize },

    /// `frame_header_size` (`H`) 不在合法区间 `1..=4096`，或 `4 + H` 超出 sample。
    #[error(
        "frame_header_size invalid: H={h} (must be 1..={max}) or 4+H > sample_size {sample_size}"
    )]
    HeaderSizeInvalid {
        h: u32,
        sample_size: usize,
        max: u32,
    },

    /// `FlatBuffers` verifier 判定 header 损坏（magic / vtable / offset 不自洽）。
    #[error("FrameHeader FlatBuffers verify failed: {reason}")]
    HeaderVerifyFailed { reason: FlatbufferFailureKind },

    /// body 区间越界：`body_offset < header_end` 或 `body_offset + body_size > sample_size`。
    #[error(
        "body out of bounds: body_offset={body_offset}, body_size={body_size}, header_end={header_end}, sample_size={sample_size}"
    )]
    BodyOutOfBounds {
        body_offset: u32,
        body_size: u32,
        header_end: u32,
        sample_size: usize,
    },

    /// blob 区间越界：`blob_offset + blob_size > sample_size`。
    #[error(
        "blob out of bounds: blob_offset={blob_offset}, blob_size={blob_size}, sample_size={sample_size}"
    )]
    BlobOutOfBounds {
        blob_offset: u32,
        blob_size: u32,
        sample_size: usize,
    },

    /// body 区间与 blob 区间重叠。
    #[error(
        "body and blob overlap: body=[{body_start},{body_end}), blob=[{blob_start},{blob_end})"
    )]
    BlobOverlap {
        body_start: u32,
        body_end: u32,
        blob_start: u32,
        blob_end: u32,
    },

    /// `schema_id` 在 `SCHEMA_REGISTRY` 里不存在。
    #[error("unknown schema_id: {schema_id:#010x}")]
    UnknownSchemaId { schema_id: u32 },
}

/// `FlatBuffers` verifier 失败的粗分类；仅用于 `ParseError::HeaderVerifyFailed`
/// 的诊断文本。`flatbuffers` crate 的 `InvalidFlatbuffer` 变体多、嵌套深，
/// 这里只压缩成几个类别方便 metric 聚合。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlatbufferFailureKind {
    /// 深度 / 递归上限。
    DepthLimit,
    /// 大小 / 计数上限。
    SizeLimit,
    /// 内部 offset / vtable / 对齐 / UTF-8 等结构错误（最常见）。
    InvalidStructure,
    /// flatbuffers crate 自身新增的错误类别（前向兼容兜底）。
    Other,
}

impl core::fmt::Display for FlatbufferFailureKind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::DepthLimit => f.write_str("depth_limit"),
            Self::SizeLimit => f.write_str("size_limit"),
            Self::InvalidStructure => f.write_str("invalid_structure"),
            Self::Other => f.write_str("other"),
        }
    }
}

impl From<InvalidFlatbuffer> for FlatbufferFailureKind {
    fn from(e: InvalidFlatbuffer) -> Self {
        // `InvalidFlatbuffer` 非 `#[non_exhaustive]`：不留 `_ =>` wildcard，
        // 将来 flatbuffers 加 variant 会触发编译失败，逼迫我们同步更新本
        // 分类 + 监控字段（保留 `Other` 给遗漏兜底保留语义槽位）。
        match e {
            InvalidFlatbuffer::DepthLimitReached => Self::DepthLimit,
            InvalidFlatbuffer::TooManyTables | InvalidFlatbuffer::ApparentSizeTooLarge => {
                Self::SizeLimit
            }
            InvalidFlatbuffer::SignedOffsetOutOfBounds { .. }
            | InvalidFlatbuffer::RangeOutOfBounds { .. }
            | InvalidFlatbuffer::InconsistentUnion { .. }
            | InvalidFlatbuffer::Utf8Error { .. }
            | InvalidFlatbuffer::MissingNullTerminator { .. }
            | InvalidFlatbuffer::Unaligned { .. }
            | InvalidFlatbuffer::MissingRequiredField { .. } => Self::InvalidStructure,
        }
    }
}

/// 按 `docs/00_design.md` §2.2 的 9 步验证序解析一条 sample。
///
/// 成功返回 [`ParsedSample`]；失败返回分类 [`ParseError`]。函数**不记录
/// metric**，调用方拿到 `Err` 后可手动 `metrics.record(&e)`，见
/// `crate::metrics::ParseMetrics::record`。
///
/// 函数不分配、不 panic、不 unwrap；可在 non-RT subscriber 线程调用。
pub fn parse_sample(bytes: &[u8]) -> Result<ParsedSample<'_>, ParseError> {
    // Step 1 ------------------------------------------------------ sample >= 4
    let sample_size = bytes.len();
    if sample_size < 4 {
        return Err(ParseError::SizeTooSmall { size: sample_size });
    }

    // Step 2 ------------------------------------------------------ 读 H
    // `get_unchecked` 的替代：借助编译器已知 `sample_size >= 4`，直接 split。
    let (prefix, rest) = bytes.split_at(4);
    let h = u32::from_le_bytes([prefix[0], prefix[1], prefix[2], prefix[3]]);

    // Step 3 ------------------------------------------------------ 0 < H <= 4096
    if h == 0 || h > FRAME_HEADER_SIZE_MAX {
        return Err(ParseError::HeaderSizeInvalid {
            h,
            sample_size,
            max: FRAME_HEADER_SIZE_MAX,
        });
    }
    // header 区间必须落在 sample 内
    let header_len = h as usize;
    if header_len > rest.len() {
        return Err(ParseError::HeaderSizeInvalid {
            h,
            sample_size,
            max: FRAME_HEADER_SIZE_MAX,
        });
    }
    let (header_bytes, _) = rest.split_at(header_len);
    let header_end: u32 = 4 + h;

    // Step 4 ------------------------------------------------------ FlatBuffers verify
    let header = flatbuffers::root::<FrameHeader>(header_bytes).map_err(|e| {
        ParseError::HeaderVerifyFailed {
            reason: FlatbufferFailureKind::from(e),
        }
    })?;

    // Step 5-6 ---------------------------------------------------- body bounds
    let body_offset = header.body_offset();
    let body_size = header.body_size();
    let body_end = check_body_bounds(body_offset, body_size, header_end, sample_size)?;

    // Step 7-8 ---------------------------------------------------- blob bounds + 不重叠
    let flags = header.capability_flags();
    let blob_slice = if has(flags, CAP_HAS_BLOB) {
        Some(check_blob_bounds(
            bytes,
            header.blob_offset(),
            header.blob_size(),
            sample_size,
            body_offset,
            body_end,
        )?)
    } else {
        None
    };

    // Step 9 ------------------------------------------------------ schema_id registry
    let schema_id = header.schema_id();
    let schema_name = lookup_name(schema_id).ok_or(ParseError::UnknownSchemaId { schema_id })?;

    Ok(ParsedSample {
        header,
        body: &bytes[body_offset as usize..body_end as usize],
        blob: blob_slice,
        schema_id,
        schema_name,
    })
}

/// Step 5-6：校验 body 区间并返回 `body_end`。
///
/// 失败一律归入 [`ParseError::BodyOutOfBounds`]（offset 太小 / 加法溢出 /
/// `body_end` 超界），调用方只需跟随 `?` 传播。
fn check_body_bounds(
    body_offset: u32,
    body_size: u32,
    header_end: u32,
    sample_size: usize,
) -> Result<u32, ParseError> {
    let make_err = || ParseError::BodyOutOfBounds {
        body_offset,
        body_size,
        header_end,
        sample_size,
    };
    if body_offset < header_end {
        return Err(make_err());
    }
    let Some(body_end) = body_offset.checked_add(body_size) else {
        return Err(make_err());
    };
    if (body_end as usize) > sample_size {
        return Err(make_err());
    }
    Ok(body_end)
}

/// Step 7-8：校验 blob 区间 + 不重叠，返回 blob 借用切片。
fn check_blob_bounds(
    bytes: &[u8],
    blob_offset: u32,
    blob_size: u32,
    sample_size: usize,
    body_offset: u32,
    body_end: u32,
) -> Result<&[u8], ParseError> {
    let bound_err = || ParseError::BlobOutOfBounds {
        blob_offset,
        blob_size,
        sample_size,
    };
    let Some(blob_end) = blob_offset.checked_add(blob_size) else {
        return Err(bound_err());
    };
    if (blob_end as usize) > sample_size {
        return Err(bound_err());
    }
    // 不重叠条件：body 整体 ≤ blob_start 或 blob 整体 ≤ body_start。
    if !(body_end <= blob_offset || blob_end <= body_offset) {
        return Err(ParseError::BlobOverlap {
            body_start: body_offset,
            body_end,
            blob_start: blob_offset,
            blob_end,
        });
    }
    Ok(&bytes[blob_offset as usize..blob_end as usize])
}
