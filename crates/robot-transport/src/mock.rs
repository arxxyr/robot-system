//! 基于内存 `Vec<u8>` 的 mock 后端。
//!
//! Step 4 的唯一后端；Step 5 起加 iceoryx2 变体，mock 继续保留用于测试。
//! 取 `latest-sample` 语义（keep-last, depth=1，§2.4 的 S0 state/command 通道
//! 策略），不模拟 history / keep-all。
//!
//! ## 观测 API
//!
//! [`MockChannel`] 暴露三个计数器供 test 断言资源守恒：
//!
//! - `publishes()` —— 成功 publish 的 sample 数
//! - `active_leases()` —— 当前存活的 [`SampleLease`](crate::sample::SampleLease) 数
//! - `active_writers()` —— 当前未 publish / discard 的 [`SampleWriter`](crate::writer::SampleWriter) 数
//!
//! 这些用于验证 "lease drop 归还 loan" / "writer drop without publish = discard"
//! 两条 §4.3 硬约束。

#![cfg(feature = "mock")]
// mock 后端的一些 fn 当前不会失败（loan / latest），但签名保留 Result 以对齐
// iceoryx2 后端的 NoFreeSample / Empty / Backend 错误分支；豁免
// unnecessary_wraps。items_after_statements 出现在 build_sample 的局部 const，
// 紧挨使用点比提到模块顶部更易读，也豁免。
#![allow(clippy::unnecessary_wraps, clippy::items_after_statements)]

use core::marker::PhantomData;
use core::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use flatbuffers::FlatBufferBuilder;
use robot_msg::capability::{CAP_HAS_BLOB, CAP_LITTLE_ENDIAN};
use robot_msg::markers::BodyMarker;
use robot_msg::msg::{FrameHeader as FbFrameHeader, FrameHeaderArgs};

use crate::error::TransportError;

// ---------------------------------------------------------------------------
// MockChannel —— 一条 "单 publisher + 单 subscriber" 的逻辑通道
// ---------------------------------------------------------------------------

/// Mock 通道：持最近一份 sample（latest slot）+ 观测计数。
///
/// 一个通道表示一个固定逻辑发布端口；Clone 与重复 `publisher()` 共享此端口。
/// 独立逻辑发布者必须各自创建通道并提供自己的身份，没有修改通道身份的 setter。
#[derive(Debug, Clone)]
pub struct MockChannel<T: BodyMarker> {
    inner: Arc<MockChannelInner>,
    _ty: PhantomData<T>,
}

impl<T: BodyMarker> Default for MockChannel<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: BodyMarker> MockChannel<T> {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(MockChannelInner::default()),
            _ty: PhantomData,
        }
    }

    /// 创建显式发布身份的 mock 通道；同一逻辑通道表示一个发布端口。
    ///
    /// 原 `new` / `default` 通道保持未知身份，不能用默认零值冒充已见来源。
    #[must_use]
    pub fn with_publisher_origin(origin: crate::PublisherOrigin) -> Self {
        Self {
            inner: Arc::new(MockChannelInner {
                publisher_origin: Some(origin),
                ..MockChannelInner::default()
            }),
            _ty: PhantomData,
        }
    }

    #[must_use]
    pub fn publisher(&self) -> MockPublisher<T> {
        MockPublisher {
            inner: Arc::clone(&self.inner),
            loan_tok: 0,
            _ty: PhantomData,
        }
    }

    #[must_use]
    pub fn subscriber(&self) -> MockSubscriber<T> {
        MockSubscriber {
            inner: Arc::clone(&self.inner),
            _ty: PhantomData,
        }
    }

    // ---- 观测 ----

    #[must_use]
    pub fn publishes(&self) -> u64 {
        self.inner.publish_count.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn loans(&self) -> u64 {
        self.inner.loan_count.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn active_leases(&self) -> i64 {
        self.inner.active_leases.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn active_writers(&self) -> i64 {
        self.inner.active_writers.load(Ordering::Relaxed)
    }
}

#[derive(Debug, Default)]
pub(crate) struct MockChannelInner {
    publisher_origin: Option<crate::PublisherOrigin>,
    slot: Mutex<Option<Arc<MockPublishedSample>>>,
    loan_count: AtomicU64,
    publish_count: AtomicU64,
    active_leases: AtomicI64,
    active_writers: AtomicI64,
}

/// 发布后 payload 与身份共同不可变；旧 lease 不从通道重新读取身份。
#[derive(Debug)]
struct MockPublishedSample {
    bytes: Vec<u8>,
    publisher_origin: Option<crate::PublisherOrigin>,
}

// ---------------------------------------------------------------------------
// MockPublisher / MockSubscriber
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct MockPublisher<T: BodyMarker> {
    inner: Arc<MockChannelInner>,
    /// 占位字段：`loan()` 返回的 writer 通过 `&'a mut self.loan_tok` 把
    /// `&mut self` 的 partial borrow 固化，阻止 writer 存活期再 `loan()`。
    ///
    /// 用 `u8` 而非 `()`：ZST borrow 在 Rust borrow checker 下被特殊放行
    /// （多个 `&mut ()` 可并存），达不到锁定效果。`u8` 是最小的非 ZST
    /// 类型，完整参与借用检查。
    loan_tok: u8,
    _ty: PhantomData<T>,
}

impl<T: BodyMarker> MockPublisher<T> {
    /// 分配一个新 writer；`active_writers` +1，`loan_count` +1。
    ///
    /// `max_sample_size` 是调用方承诺的 sample 字节上限；publish 时若合成的
    /// sample 超过此值会返 [`TransportError::SampleTooLarge`]。mock 此处不做
    /// 底层分配（body / blob 按需 push 进 `Vec<u8>`），仅记录上限。
    pub(crate) fn mock_loan(
        &mut self,
        max_sample_size: usize,
    ) -> Result<MockSampleWriter, TransportError> {
        self.inner.loan_count.fetch_add(1, Ordering::Relaxed);
        self.inner.active_writers.fetch_add(1, Ordering::Relaxed);
        Ok(MockSampleWriter {
            body_bytes: Vec::new(),
            blob_bytes: None,
            stamp_ns: 0,
            source_id: 0,
            seq: 0,
            schema_id: T::SCHEMA_ID,
            max_sample_size,
            channel: Arc::clone(&self.inner),
            consumed: false,
        })
    }

    /// 暴露占位字段给 `SampleWriter::from_mock(mock, pub.loan_borrow())` 使用。
    pub(crate) fn loan_borrow(&mut self) -> &mut u8 {
        &mut self.loan_tok
    }
}

#[derive(Debug, Clone)]
pub struct MockSubscriber<T: BodyMarker> {
    inner: Arc<MockChannelInner>,
    _ty: PhantomData<T>,
}

impl<T: BodyMarker> MockSubscriber<T> {
    /// 取 latest slot；若无 sample 返回 `None`。`active_leases` +1。
    pub(crate) fn mock_latest(&mut self) -> Result<Option<MockSampleLease>, TransportError> {
        let slot = self
            .inner
            .slot
            .lock()
            .map_err(|e| TransportError::Backend(format!("slot mutex poisoned: {e}")))?;
        match slot.as_ref() {
            None => Ok(None),
            Some(sample) => {
                let lease = MockSampleLease {
                    sample: Arc::clone(sample),
                    channel: Arc::clone(&self.inner),
                };
                self.inner.active_leases.fetch_add(1, Ordering::Relaxed);
                Ok(Some(lease))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// MockSampleWriter —— 持 body / blob buffer，publish 时合成完整 sample
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct MockSampleWriter {
    body_bytes: Vec<u8>,
    blob_bytes: Option<Vec<u8>>,
    pub(crate) stamp_ns: u64,
    pub(crate) source_id: u32,
    pub(crate) seq: u64,
    schema_id: u32,
    /// 由 `mock_loan(size)` 传入；publish 时对合成 sample 总字节做硬限检查。
    max_sample_size: usize,
    channel: Arc<MockChannelInner>,
    consumed: bool,
}

impl MockSampleWriter {
    pub(crate) fn set_body_bytes(&mut self, body: &[u8]) {
        self.body_bytes.clear();
        self.body_bytes.extend_from_slice(body);
    }

    pub(crate) fn set_body_owned(&mut self, body: Vec<u8>) {
        self.body_bytes = body;
    }

    pub(crate) fn set_blob_bytes(&mut self, blob: &[u8]) {
        self.blob_bytes = Some(blob.to_vec());
    }

    pub(crate) fn clear_blob(&mut self) {
        self.blob_bytes = None;
    }

    /// 按 §2.2 wire 格式合成完整 sample 并写入 latest slot；成功后
    /// `active_writers` -1，`publish_count` +1。
    ///
    /// 若合成字节超过 `mock_loan(size)` 承诺上限 → [`TransportError::SampleTooLarge`]
    /// （与 iceoryx2 后端 `loan_slice_uninit` 的硬限检查对齐）。
    pub(crate) fn mock_publish(mut self) -> Result<(), TransportError> {
        let bytes = self.build_sample()?;
        if bytes.len() > self.max_sample_size {
            return Err(TransportError::SampleTooLarge {
                size: bytes.len(),
                limit: self.max_sample_size,
            });
        }
        {
            let mut slot = self
                .channel
                .slot
                .lock()
                .map_err(|e| TransportError::Backend(format!("slot mutex poisoned: {e}")))?;
            *slot = Some(Arc::new(MockPublishedSample {
                bytes,
                publisher_origin: self.channel.publisher_origin,
            }));
        }
        self.channel.publish_count.fetch_add(1, Ordering::Relaxed);
        self.consumed = true; // drop 只减 writer 计数，不再当作 discard
        Ok(())
    }

    fn build_sample(&self) -> Result<Vec<u8>, TransportError> {
        let body_size = u32::try_from(self.body_bytes.len())
            .map_err(|_| TransportError::Backend("body exceeds u32".into()))?;
        let blob_size_val = match &self.blob_bytes {
            Some(b) => u32::try_from(b.len())
                .map_err(|_| TransportError::Backend("blob exceeds u32".into()))?,
            None => 0,
        };
        let mut flags = CAP_LITTLE_ENDIAN;
        if self.blob_bytes.is_some() {
            flags |= CAP_HAS_BLOB;
        }

        // 两遍 build：probe 用 body_offset / blob_offset placeholder 探测 H；
        // 其他字段 probe 和 real 完全一致（含 "是否为 0"），这样 `FlatBuffers`
        // vtable 在两遍间长度稳定。只有 body_offset 永远非 0（= 4 + H），
        // probe 用非零 placeholder；blob_offset 的 "是否为 0" 由 "是否有 blob"
        // 决定，probe 保持同样策略。
        // 详见 memory/flatc_25_pitfalls.md 坑 4（vtable 省略默认值 0）。
        const PLACEHOLDER_NONZERO: u32 = 0x0101_0101;
        let probe_blob_offset = if self.blob_bytes.is_some() {
            PLACEHOLDER_NONZERO
        } else {
            0
        };
        let probe_header = encode_header(&FrameHeaderArgs {
            magic: 0x5242_5443,
            abi_version: 1,
            schema_id: self.schema_id,
            schema_version: 1,
            capability_flags: flags,
            stamp_ns: self.stamp_ns,
            source_id: self.source_id,
            seq: self.seq,
            body_offset: PLACEHOLDER_NONZERO,
            body_size,
            blob_offset: probe_blob_offset,
            blob_size: blob_size_val,
            header_crc32: 0,
        });
        let h = u32::try_from(probe_header.len())
            .map_err(|_| TransportError::Backend("frame_header > u32::MAX".into()))?;
        let body_offset = 4 + h;
        let body_end = body_offset + body_size;
        let (blob_offset, blob_size) = match &self.blob_bytes {
            Some(_) => (body_end, blob_size_val),
            None => (0, 0),
        };
        let real = encode_header(&FrameHeaderArgs {
            magic: 0x5242_5443,
            abi_version: 1,
            schema_id: self.schema_id,
            schema_version: 1,
            capability_flags: flags,
            stamp_ns: self.stamp_ns,
            source_id: self.source_id,
            seq: self.seq,
            body_offset,
            body_size,
            blob_offset,
            blob_size,
            header_crc32: 0,
        });
        // probe / real size 必须一致，否则 body_offset 错位 —— 见上面注释
        if real.len() != probe_header.len() {
            return Err(TransportError::Backend(format!(
                "probe/real FrameHeader size mismatch ({} vs {})",
                probe_header.len(),
                real.len()
            )));
        }
        let mut out = Vec::with_capacity(
            4 + real.len() + self.body_bytes.len() + self.blob_bytes.as_ref().map_or(0, Vec::len),
        );
        out.extend_from_slice(&h.to_le_bytes());
        out.extend_from_slice(&real);
        out.extend_from_slice(&self.body_bytes);
        if let Some(b) = &self.blob_bytes {
            out.extend_from_slice(b);
        }
        Ok(out)
    }
}

impl Drop for MockSampleWriter {
    fn drop(&mut self) {
        self.channel.active_writers.fetch_sub(1, Ordering::Relaxed);
        // 未 publish（consumed == false）→ 丢弃 buffer，mock 侧没有真 loan
        // 需要归还。Iceoryx2 变体里 drop 时调用 Sample::release() 归还 shm。
    }
}

fn encode_header(args: &FrameHeaderArgs) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();
    let fh = FbFrameHeader::create(&mut fbb, args);
    fbb.finish(fh, None);
    fbb.finished_data().to_vec()
}

// ---------------------------------------------------------------------------
// MockSampleLease —— 从 latest slot 拷贝指针，drop 时 active_leases -1
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct MockSampleLease {
    sample: Arc<MockPublishedSample>,
    channel: Arc<MockChannelInner>,
}

impl MockSampleLease {
    /// 身份随实际样本保存；只复制元数据，不读通道、不增加 `Arc` 引用。
    #[inline]
    pub(crate) fn publisher_origin(&self) -> Option<crate::PublisherOrigin> {
        self.sample.publisher_origin
    }

    #[inline]
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.sample.bytes
    }
}

impl Drop for MockSampleLease {
    fn drop(&mut self) {
        self.channel.active_leases.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owned_body_retains_allocation_and_discard_releases_writer() {
        let channel = MockChannel::<robot_msg::markers::JointState>::new();
        let mut publisher = channel.publisher();
        let mut writer = publisher.mock_loan(1024).unwrap();
        for value in [1, 2] {
            let mut body = Vec::with_capacity(128);
            body.extend_from_slice(&[value; 16]);
            let allocation = (body.as_ptr(), body.capacity());
            writer.set_body_owned(body);
            assert_eq!(
                (writer.body_bytes.as_ptr(), writer.body_bytes.capacity()),
                allocation
            );
            assert_eq!(writer.body_bytes, [value; 16]);
        }
        drop(writer);
        assert_eq!((channel.active_writers(), channel.publishes()), (0, 0));
    }
}
