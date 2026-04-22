//! `SampleLease<T>` + 视图家族（[`MessageView`] / [`HeaderView`] /
//! [`BodyView`] / [`BlobView`]）。
//!
//! 对应 `docs/00_design.md` §2.5 / §4.3。
//!
//! ### 生命周期契约
//!
//! - `SampleLease<T>` 按**值语义**持有底层 sample loan；`drop` 自动归还
//!   （mock 后端下是 `active_leases -= 1`；iceoryx2 后端下是真正 release shm）。
//! - `lease.view()` 返回的 [`MessageView<'a, T>`] 借用 lease 内的 bytes，
//!   `'a` 绑定 `&lease`，Rust 借用检查保证 **view 永远不能 outlive lease**。
//! - [`BodyView::get`] 返回 typed `FlatBuffers` root，view 阶段已 verify 过；
//!   调用方可直接读字段，不必再次 verify。
//! - [`BlobView`] 只在 `T::BLOB_EMBEDDED == true` 且 `FrameHeader` 的
//!   `CAP_HAS_BLOB` 位置 1 时存在；alignment 检查在 `view()` 已做。

use core::marker::PhantomData;

use robot_msg::capability::{CAP_HAS_BLOB, has};
use robot_msg::markers::{BlobDescInfo, BodyMarker};
use robot_msg::msg::FrameHeader as FbFrameHeader;

use crate::error::TransportError;
use crate::parser::{ParseError, ParsedSample, parse_sample};

// ---------------------------------------------------------------------------
// SampleLease
// ---------------------------------------------------------------------------

/// 持有一条 sample 的 loan。按值语义：`Clone` 不开放（克隆会引起 loan 计数
/// 失衡）；`drop` 自动归还。
///
/// 类型参数 `T` 是 [`BodyMarker`] 实现（`markers::JointState` 等），用于
/// 绑定 `schema_id` 并指导 `view()` 的 typed 验证。
pub struct SampleLease<T: BodyMarker> {
    inner: SampleLeaseInner,
    _ty: PhantomData<T>,
}

#[allow(clippy::large_enum_variant)]
enum SampleLeaseInner {
    #[cfg(feature = "mock")]
    Mock(crate::mock::MockSampleLease),
    #[cfg(feature = "iceoryx2")]
    Iceoryx2(crate::iceoryx2::Iceoryx2SampleLease),
}

impl<T: BodyMarker> core::fmt::Debug for SampleLease<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SampleLease")
            .field("marker", &T::SCHEMA_NAME)
            .finish()
    }
}

impl<T: BodyMarker> SampleLease<T> {
    #[cfg(feature = "mock")]
    #[must_use]
    pub(crate) fn from_mock(mock: crate::mock::MockSampleLease) -> Self {
        Self {
            inner: SampleLeaseInner::Mock(mock),
            _ty: PhantomData,
        }
    }

    #[cfg(feature = "iceoryx2")]
    #[must_use]
    pub(crate) fn from_iceoryx2(lease: crate::iceoryx2::Iceoryx2SampleLease) -> Self {
        Self {
            inner: SampleLeaseInner::Iceoryx2(lease),
            _ty: PhantomData,
        }
    }

    #[inline]
    fn bytes(&self) -> &[u8] {
        match &self.inner {
            #[cfg(feature = "mock")]
            SampleLeaseInner::Mock(m) => m.bytes(),
            #[cfg(feature = "iceoryx2")]
            SampleLeaseInner::Iceoryx2(l) => l.bytes(),
        }
    }

    /// 对 sample 执行完整验证并返回 typed 视图。
    ///
    /// 执行顺序：
    /// 1. [`parse_sample`] 的 9 步（Step 3）—— header / body / blob bounds
    /// 2. `schema_id` 与 `T::SCHEMA_ID` 对比 → [`TransportError::SchemaMismatch`]
    /// 3. body 作为 `T::Root<'_>` `FlatBuffers` verify
    /// 4. 若 `T::BLOB_EMBEDDED`：读 `BlobDesc.alignment` 并校验
    ///    `blob_offset % alignment == 0` → [`TransportError::BlobMisaligned`]
    ///
    /// 返回的 [`MessageView`] 借用 `&self`；借用检查保证不能 outlive lease。
    pub fn view(&self) -> Result<MessageView<'_, T>, TransportError> {
        let bytes = self.bytes();
        let parsed: ParsedSample<'_> = parse_sample(bytes)?;

        // 2. schema_id 匹配
        if parsed.schema_id != T::SCHEMA_ID {
            return Err(TransportError::SchemaMismatch {
                expected: T::SCHEMA_ID,
                expected_name: T::SCHEMA_NAME,
                actual: parsed.schema_id,
                actual_name: parsed.schema_name,
            });
        }

        // 3. typed body verify
        let body_root = flatbuffers::root::<T::Root<'_>>(parsed.body).map_err(|e| {
            TransportError::BodyVerifyFailed {
                marker: T::SCHEMA_NAME,
                cause: e.to_string(),
            }
        })?;

        // 4. blob alignment（若 T 声明带 blob 且 sample 确实携带 blob）
        let blob = match (T::BLOB_EMBEDDED, parsed.blob) {
            (true, Some(raw_blob)) => {
                let Some(desc) = T::read_blob_desc(&body_root) else {
                    // marker 声明 BLOB_EMBEDDED 但 body 里 blob 字段缺失 —— 发布方 bug。
                    return Err(TransportError::BodyVerifyFailed {
                        marker: T::SCHEMA_NAME,
                        cause: "BLOB_EMBEDDED = true but body has no BlobDesc field".into(),
                    });
                };
                let blob_offset = parsed.header.blob_offset();
                let alignment = desc.alignment;
                if alignment > 0 && !blob_offset.is_multiple_of(u32::from(alignment)) {
                    return Err(TransportError::BlobMisaligned {
                        blob_offset,
                        alignment,
                    });
                }
                Some(BlobView {
                    bytes: raw_blob,
                    desc,
                })
            }
            // T 不带 blob，但 sample 却写了 CAP_HAS_BLOB —— 按 §2.2 是 publisher
            // 违规；这里保守地**接受**：对 body 层不可见的 blob 区域不做 typed
            // 校验。调用方可以用 `view.header.capability_flags()` 自检。
            (false, _) => None,
            // T 声明带 blob 但 sample 没携带 —— 视为发布方合同违反。
            (true, None) => {
                return Err(TransportError::BodyVerifyFailed {
                    marker: T::SCHEMA_NAME,
                    cause: format!(
                        "marker declares BLOB_EMBEDDED but sample has no blob (flags={:#x})",
                        parsed.header.capability_flags()
                    ),
                });
            }
        };

        Ok(MessageView {
            header: HeaderView {
                inner: parsed.header,
            },
            body: BodyView {
                root: body_root,
                bytes: parsed.body,
                _ty: PhantomData,
            },
            blob,
        })
    }

    /// 访问底层 [`FrameHeader`](robot_msg::msg::FrameHeader) 的便利方式
    /// （不做 body / blob 校验）。
    pub fn header(&self) -> Result<HeaderView<'_>, ParseError> {
        Ok(HeaderView {
            inner: parse_sample(self.bytes())?.header,
        })
    }
}

// ---------------------------------------------------------------------------
// 视图家族
// ---------------------------------------------------------------------------

/// 完整 typed sample 视图。字段借用来自 lease 的 sample bytes。
#[derive(Debug)]
pub struct MessageView<'a, T: BodyMarker> {
    pub header: HeaderView<'a>,
    pub body: BodyView<'a, T>,
    pub blob: Option<BlobView<'a>>,
}

impl<T: BodyMarker> MessageView<'_, T> {
    /// `FrameHeader.capability_flags` 的 [`CAP_HAS_BLOB`] 位。
    #[must_use]
    pub fn has_blob_flag(&self) -> bool {
        has(self.header.inner.capability_flags(), CAP_HAS_BLOB)
    }
}

/// `FrameHeader` 的只读视图。直接暴露 `FlatBuffers` table handle，调用方可
/// 调其 accessor（`stamp_ns()` / `seq()` / `schema_id()` 等）。
#[derive(Debug, Clone, Copy)]
pub struct HeaderView<'a> {
    pub(crate) inner: FbFrameHeader<'a>,
}

impl<'a> HeaderView<'a> {
    #[inline]
    #[must_use]
    pub fn get(&self) -> FbFrameHeader<'a> {
        self.inner
    }

    #[inline]
    #[must_use]
    pub fn schema_id(&self) -> u32 {
        self.inner.schema_id()
    }

    #[inline]
    #[must_use]
    pub fn stamp_ns(&self) -> u64 {
        self.inner.stamp_ns()
    }

    #[inline]
    #[must_use]
    pub fn seq(&self) -> u64 {
        self.inner.seq()
    }

    #[inline]
    #[must_use]
    pub fn source_id(&self) -> u32 {
        self.inner.source_id()
    }

    #[inline]
    #[must_use]
    pub fn capability_flags(&self) -> u64 {
        self.inner.capability_flags()
    }
}

/// Body 的 typed 视图 —— 已 verify 过的 `FlatBuffers` root table。
pub struct BodyView<'a, T: BodyMarker> {
    root: T::Root<'a>,
    bytes: &'a [u8],
    _ty: PhantomData<T>,
}

impl<T: BodyMarker> core::fmt::Debug for BodyView<'_, T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BodyView")
            .field("marker", &T::SCHEMA_NAME)
            .field("size", &self.bytes.len())
            .finish()
    }
}

impl<'a, T: BodyMarker> BodyView<'a, T> {
    /// 返回已验证的 `FlatBuffers` root table。
    #[inline]
    #[must_use]
    pub fn get(&self) -> T::Root<'a> {
        self.root
    }

    /// 原始 body 字节（长度 = `FrameHeader.body_size`）。
    #[inline]
    #[must_use]
    pub fn raw(&self) -> &'a [u8] {
        self.bytes
    }
}

/// Blob 的只读视图。
#[derive(Debug, Clone, Copy)]
pub struct BlobView<'a> {
    pub(crate) bytes: &'a [u8],
    pub(crate) desc: BlobDescInfo,
}

impl<'a> BlobView<'a> {
    #[inline]
    #[must_use]
    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    #[inline]
    #[must_use]
    pub fn desc(&self) -> BlobDescInfo {
        self.desc
    }
}
