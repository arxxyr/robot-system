//! `SampleWriter<'a, T>` —— publisher loan 出来的可写 sample 句柄。
//!
//! 对应 `docs/00_design.md` §4.3 + `docs/CHANGELOG.md` 2026-04-22 "§4.3
//! writer API 调整"：`publish` 方法挂在 writer 自身（按值消费 `self`），
//! 而非 `Publisher::publish(&mut self, w)`。这样 writer 持 `&'a mut
//! Publisher` 的 borrow 自然在 publish move 的那一刻释放。
//!
//! ### 语义不变条款（§4.3 原保证）
//!
//! - `loan()` 得 writer 后，`&'a mut Publisher` borrow 被 writer 占用。
//! - **drop without publish** → `SampleWriter::drop` 自动 discard；mock 下
//!   减 `active_writers`，iceoryx2 下真 release shm loan。
//! - **publish(self)** → 消费 writer，写入 service slot，`active_writers`
//!   同样 -1 但 `publish_count` +1。
//! - 错误路径直接 `?` 传播 → writer drop → discard 同步归还 loan。

use core::marker::PhantomData;

use robot_msg::markers::BodyMarker;

use crate::error::TransportError;

/// 本次发布的真实后端结果；不代表订阅方消费、执行或持久化。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishReceipt {
    /// iceoryx2 实际成功投递的连接数，零也表示发布成功。
    Iceoryx2 { recipients: usize },
    /// mock 仅保存 latest slot，不提供 history 或订阅者计数。
    MockStored,
}

/// 失败发生在本帧进入 history 之前，或不能证明尚未产生副作用。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PublishFailure {
    #[error(transparent)]
    BeforeHistory(TransportError),
    #[error(transparent)]
    Uncertain(TransportError),
}

impl PublishFailure {
    /// 按值返回原错误；兼容旧 publish 的错误类型与原始诊断内容。
    #[must_use]
    pub fn into_transport(self) -> TransportError {
        match self {
            Self::BeforeHistory(error) | Self::Uncertain(error) => error,
        }
    }
}

/// 可写 sample 句柄。Step 4 只暴露 body / blob 字节级写入 + publish；Step 5
/// 起追加 typed builder 辅助（`set_body_with<F: FnOnce(&mut FlatBufferBuilder)>`）。
///
/// `'a` 绑定到 `Publisher` 内部的一个占位字段的 `&'a mut u8` 借用（mock 下是
/// `MockPublisher::loan_tok`），**实际**占用 publisher 的可变借用，阻止
/// writer 存活期间再 `loan()`（见 `tests/ui/second_loan_before_publish.rs`）。
///
/// **三个坑，按踩过的顺序列出**：
///
/// 1. `PhantomData<&'a mut ()>` 不行 —— lifetime 可以 subtype 到任意短，
///    borrow check 不会把 publisher 算作被借。必须持有**真实** `&'a mut _` 字段。
/// 2. `&'a mut ()` 不行 —— ZST 引用被 Rust 特殊处理，多个 `&mut ()` 允许并存。
/// 3. `&'a mut u8` 可以 —— 最小非 ZST 类型，完整参与借用检查。
pub struct SampleWriter<'a, T: BodyMarker> {
    inner: SampleWriterInner<'a>,
    _borrow: &'a mut u8,
    _ty: PhantomData<T>,
}

#[allow(clippy::large_enum_variant)] // Mock 与 Iceoryx2 变体大小量级接近，保持 inline。
enum SampleWriterInner<'a> {
    #[cfg(feature = "mock")]
    Mock(crate::mock::MockSampleWriter, PhantomData<&'a ()>),
    #[cfg(feature = "iceoryx2")]
    Iceoryx2(crate::iceoryx2::Iceoryx2SampleWriter<'a>),
    // 无后端时仍保持 lifetime 与不可构造性，不增可用的伪 publisher。
    #[cfg(not(any(feature = "mock", feature = "iceoryx2")))]
    #[allow(dead_code)]
    Unavailable(core::convert::Infallible, PhantomData<&'a ()>),
}

impl<T: BodyMarker> core::fmt::Debug for SampleWriter<'_, T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SampleWriter")
            .field("marker", &T::SCHEMA_NAME)
            .finish()
    }
}

impl<'a, T: BodyMarker> SampleWriter<'a, T> {
    /// `publisher_borrow` 是 publisher 内部占位字段（非 ZST `u8`）的
    /// `&'a mut u8` 借用；它被实际存进 writer 里（非 PhantomData），编译期
    /// 强制 publisher 在 writer 存活期内被借用、不能再 `loan()`。
    #[cfg(feature = "mock")]
    pub(crate) fn from_mock(
        mock: crate::mock::MockSampleWriter,
        publisher_borrow: &'a mut u8,
    ) -> Self {
        Self {
            inner: SampleWriterInner::Mock(mock, PhantomData),
            _borrow: publisher_borrow,
            _ty: PhantomData,
        }
    }

    #[cfg(feature = "iceoryx2")]
    pub(crate) fn from_iceoryx2(
        w: crate::iceoryx2::Iceoryx2SampleWriter<'a>,
        publisher_borrow: &'a mut u8,
    ) -> Self {
        Self {
            inner: SampleWriterInner::Iceoryx2(w),
            _borrow: publisher_borrow,
            _ty: PhantomData,
        }
    }

    /// 写入 body 字节（应为一段完整的 `FlatBuffers` root table，对应 `T::Root`）。
    /// 重复调用覆盖上一次。
    pub fn set_body_bytes(&mut self, body: &[u8]) {
        #[cfg(not(any(feature = "mock", feature = "iceoryx2")))]
        let _ = body;
        match &mut self.inner {
            #[cfg(feature = "mock")]
            SampleWriterInner::Mock(m, _) => m.set_body_bytes(body),
            #[cfg(feature = "iceoryx2")]
            SampleWriterInner::Iceoryx2(w) => w.set_body_bytes(body),
            #[cfg(not(any(feature = "mock", feature = "iceoryx2")))]
            SampleWriterInner::Unavailable(never, _) => match *never {},
        }
    }

    /// 写入 blob 字节（raw payload；由调用方保证 alignment / dtype / size 一致）。
    pub fn set_blob_bytes(&mut self, blob: &[u8]) {
        #[cfg(not(any(feature = "mock", feature = "iceoryx2")))]
        let _ = blob;
        match &mut self.inner {
            #[cfg(feature = "mock")]
            SampleWriterInner::Mock(m, _) => m.set_blob_bytes(blob),
            #[cfg(feature = "iceoryx2")]
            SampleWriterInner::Iceoryx2(w) => w.set_blob_bytes(blob),
            #[cfg(not(any(feature = "mock", feature = "iceoryx2")))]
            SampleWriterInner::Unavailable(never, _) => match *never {},
        }
    }

    /// 清除 blob —— 下一次 `publish` 将产出一条无 blob sample。
    pub fn clear_blob(&mut self) {
        match &mut self.inner {
            #[cfg(feature = "mock")]
            SampleWriterInner::Mock(m, _) => m.clear_blob(),
            #[cfg(feature = "iceoryx2")]
            SampleWriterInner::Iceoryx2(w) => w.clear_blob(),
            #[cfg(not(any(feature = "mock", feature = "iceoryx2")))]
            SampleWriterInner::Unavailable(never, _) => match *never {},
        }
    }

    /// 设置 `FrameHeader.stamp_ns` —— 调用方自己用 [`robot_core::time::RobotClock`]
    /// 生成，禁止直接 `Instant::now()`（业务代码禁令，见 `AGENTS.md`）。
    pub fn set_stamp_ns(&mut self, stamp_ns: u64) {
        #[cfg(not(any(feature = "mock", feature = "iceoryx2")))]
        let _ = stamp_ns;
        match &mut self.inner {
            #[cfg(feature = "mock")]
            SampleWriterInner::Mock(m, _) => m.stamp_ns = stamp_ns,
            #[cfg(feature = "iceoryx2")]
            SampleWriterInner::Iceoryx2(w) => w.stamp_ns = stamp_ns,
            #[cfg(not(any(feature = "mock", feature = "iceoryx2")))]
            SampleWriterInner::Unavailable(never, _) => match *never {},
        }
    }

    /// 设置 `FrameHeader.source_id`。
    pub fn set_source_id(&mut self, source_id: u32) {
        #[cfg(not(any(feature = "mock", feature = "iceoryx2")))]
        let _ = source_id;
        match &mut self.inner {
            #[cfg(feature = "mock")]
            SampleWriterInner::Mock(m, _) => m.source_id = source_id,
            #[cfg(feature = "iceoryx2")]
            SampleWriterInner::Iceoryx2(w) => w.source_id = source_id,
            #[cfg(not(any(feature = "mock", feature = "iceoryx2")))]
            SampleWriterInner::Unavailable(never, _) => match *never {},
        }
    }

    /// 设置 `FrameHeader.seq` —— 发布方负责单调递增。
    pub fn set_seq(&mut self, seq: u64) {
        #[cfg(not(any(feature = "mock", feature = "iceoryx2")))]
        let _ = seq;
        match &mut self.inner {
            #[cfg(feature = "mock")]
            SampleWriterInner::Mock(m, _) => m.seq = seq,
            #[cfg(feature = "iceoryx2")]
            SampleWriterInner::Iceoryx2(w) => w.seq = seq,
            #[cfg(not(any(feature = "mock", feature = "iceoryx2")))]
            SampleWriterInner::Unavailable(never, _) => match *never {},
        }
    }

    /// 按值消费 `self` 发布 sample。
    ///
    /// 成功后：publisher 的 `&'a mut` borrow 释放（writer moved），调用方可
    /// 立即 `publisher.loan()` 取下一个 writer。失败时也消费 self（错误已
    /// 传出，不需要复用 writer）。
    pub fn publish(self) -> Result<(), TransportError> {
        match self.inner {
            #[cfg(feature = "mock")]
            SampleWriterInner::Mock(m, _) => m.mock_publish(),
            #[cfg(feature = "iceoryx2")]
            SampleWriterInner::Iceoryx2(w) => w.iceoryx2_publish(),
            #[cfg(not(any(feature = "mock", feature = "iceoryx2")))]
            SampleWriterInner::Unavailable(never, _) => match never {},
        }
    }

    /// 消费本 writer，返回同一次真实发送的结果；失败同样归还本地 loan。
    pub fn publish_typed(self) -> Result<PublishReceipt, PublishFailure> {
        match self.inner {
            #[cfg(feature = "mock")]
            SampleWriterInner::Mock(mock, _) => mock
                .mock_publish()
                .map(|()| PublishReceipt::MockStored)
                .map_err(PublishFailure::BeforeHistory),
            #[cfg(feature = "iceoryx2")]
            SampleWriterInner::Iceoryx2(writer) => writer
                .publish_impl()
                .map(|recipients| PublishReceipt::Iceoryx2 { recipients }),
            #[cfg(not(any(feature = "mock", feature = "iceoryx2")))]
            SampleWriterInner::Unavailable(never, _) => match never {},
        }
    }
}
