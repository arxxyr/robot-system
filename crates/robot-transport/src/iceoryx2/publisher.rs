//! `Iceoryx2Publisher<T>` + `Iceoryx2SampleWriter<'a>` —— 真 iceoryx2 后端
//! 的发布侧。
//!
//! ### 延迟 loan 策略
//!
//! 变长 sample（`FrameHeader` + body + 可选 blob）的字节数要等 body / blob
//! 都写完才能确定。iceoryx2 `loan_slice_uninit(n)` 要求精确元素数（`n` 即
//! sample 字节数），不能 loan 大再 truncate。因此本实现**不在 `loan()` 时
//! 真向 iceoryx2 申请 shm**，而是：
//!
//! 1. `loan(size)` 只构造 writer，记录 `max_sample_size` 作为 publish 上限。
//! 2. `publish()` 时按 §2.2 wire 格式合成完整字节 → 检查 ≤ `max_sample_size`
//!    → 调 `publisher.loan_slice_uninit(exact_size)` → `write_from_slice` 拷
//!    一次 → `send()`。
//!
//! 代价：publish 时一次内存到 shm 的拷贝（`write_from_slice`）。Step 5 MVP
//! 可接受；Step 9+ 性能调优时再评估"直接写 uninit slice + 预留 header"的
//! 零拷贝路径。
//!
//! ### Borrow 语义
//!
//! `Iceoryx2SampleWriter<'a>` 持 `&'a mut IxPublisher` —— `'a` 与外层
//! `SampleWriter<'a, T>::_borrow: &'a mut u8` 同寿，publisher 在 writer 存活
//! 期间被完全锁住（不能再 `loan()`），iceoryx2 publisher 本身无并发 loan 限
//! 制但我们借此保证 Publisher trait 的语义统一。

use core::marker::PhantomData;

use flatbuffers::FlatBufferBuilder;
use iceoryx2::port::publisher::Publisher as IxPublisher;
use iceoryx2::service::ipc::Service as IxService;
use robot_msg::capability::{CAP_HAS_BLOB, CAP_LITTLE_ENDIAN};
use robot_msg::markers::BodyMarker;
use robot_msg::msg::{FrameHeader as FbFrameHeader, FrameHeaderArgs};

use crate::error::TransportError;
use crate::writer::PublishFailure;

/// iceoryx2 payload 统一用 `[u8]`（变长切片）+ `UserHeader = ()`。
pub(crate) type IxPubSubPublisher = IxPublisher<IxService, [u8], ()>;

/// 真后端发布者，按 `BodyMarker` 泛型化。
///
/// 创建通道：由 [`crate::iceoryx2::open_or_create_publisher`] /
/// [`crate::iceoryx2::open_or_create_subscriber`] 构造（Step 5 下一 commit 添）。
/// 本 commit 只落 trait impl 所需类型。
pub struct Iceoryx2Publisher<T: BodyMarker> {
    pub(crate) inner: IxPubSubPublisher,
    /// 同 `MockPublisher::loan_tok`：placeholder `&'a mut u8` borrow，编译期
    /// 阻止 writer 存活期间再次 `loan()`。
    pub(crate) loan_tok: u8,
    pub(crate) _ty: PhantomData<T>,
}

impl<T: BodyMarker> core::fmt::Debug for Iceoryx2Publisher<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Iceoryx2Publisher")
            .field("marker", &T::SCHEMA_NAME)
            .finish()
    }
}

impl<T: BodyMarker> Iceoryx2Publisher<T> {
    /// 同时拿 `(publisher ref, loan_tok)` 的 disjoint field borrow。
    ///
    /// `Publisher::loan` 需要两个不相交借用：一个给 writer 做延迟 loan，一个
    /// 给外层 `SampleWriter::_borrow: &'a mut u8` 做编译期锁。直接分开调
    /// 两个 `&mut self` 方法会被 borrow checker 拒绝，一次性 disjoint split
    /// 规避。
    pub(crate) fn split_for_loan(&mut self) -> (&mut IxPubSubPublisher, &mut u8) {
        (&mut self.inner, &mut self.loan_tok)
    }

    /// 显式刷新 subscriber 连接并向**新连上的** subscriber 补发 history。
    ///
    /// iceoryx2 只在 `publish` / `update_connections` 时把 history 投递给
    /// 新 subscriber——publisher 发完最后一帧后才连上的 subscriber，若没
    /// 有人再调本方法，将**永远**收不到 history（0.9.3 实测；上游
    /// `port/update_connections.rs` doc 示例即此场景）。短命 per-topic
    /// publisher（如 s1 `events_drain` terminal grace 窗口）必须周期调用，
    /// 否则晚连订阅者空等。长命持续发布的 topic 不需要（每次 publish 自带
    /// connection update）。
    ///
    /// # Errors
    ///
    /// `TransportError::Backend`：iceoryx2 建立连接失败（shm 资源耗尽等）。
    pub fn update_connections(&self) -> Result<(), TransportError> {
        use iceoryx2::port::update_connections::UpdateConnections;
        self.inner.update_connections().map_err(|e| {
            TransportError::Backend(format!("iceoryx2 publisher update_connections: {e:?}"))
        })
    }
}

/// 发布侧的 sample writer。`'a` 来自 `&'a mut Iceoryx2Publisher` 的 borrow。
///
/// body / blob / metadata 先暂存在 `Vec<u8>`，publish 时一次性合成并拷入
/// iceoryx2 loan。drop without publish = 不调 iceoryx2（buffer 丢弃即可）。
pub struct Iceoryx2SampleWriter<'a> {
    publisher: &'a mut IxPubSubPublisher,
    schema_id: u32,
    schema_version: u16,
    max_sample_size: usize,
    body_bytes: Vec<u8>,
    blob_bytes: Option<Vec<u8>>,
    pub(crate) stamp_ns: u64,
    pub(crate) source_id: u32,
    pub(crate) seq: u64,
    consumed: bool,
}

impl core::fmt::Debug for Iceoryx2SampleWriter<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Iceoryx2SampleWriter")
            .field("schema_id", &format_args!("{:#010x}", self.schema_id))
            .field("max_sample_size", &self.max_sample_size)
            .field("body_len", &self.body_bytes.len())
            .field("blob_len", &self.blob_bytes.as_ref().map_or(0, Vec::len))
            .field("consumed", &self.consumed)
            .finish()
    }
}

impl<'a> Iceoryx2SampleWriter<'a> {
    /// 从已 disjoint split 出的 `&'a mut IxPubSubPublisher` 构造 writer。
    /// 调用方见 `Publisher::loan` 实现（`traits.rs`）。
    pub(crate) fn new_disjoint(
        publisher: &'a mut IxPubSubPublisher,
        schema_id: u32,
        max_sample_size: usize,
    ) -> Self {
        Self {
            publisher,
            schema_id,
            schema_version: 1,
            max_sample_size,
            body_bytes: Vec::new(),
            blob_bytes: None,
            stamp_ns: 0,
            source_id: 0,
            seq: 0,
            consumed: false,
        }
    }
}

impl Iceoryx2SampleWriter<'_> {
    pub(crate) fn set_body_bytes(&mut self, body: &[u8]) {
        self.body_bytes.clear();
        self.body_bytes.extend_from_slice(body);
    }

    pub(crate) fn set_blob_bytes(&mut self, blob: &[u8]) {
        self.blob_bytes = Some(blob.to_vec());
    }

    pub(crate) fn clear_blob(&mut self) {
        self.blob_bytes = None;
    }

    pub(crate) fn iceoryx2_publish(self) -> Result<(), TransportError> {
        self.publish_impl()
            .map(|_| ())
            .map_err(PublishFailure::into_transport)
    }

    pub(crate) fn publish_impl(mut self) -> Result<usize, PublishFailure> {
        let wire = self.build_sample().map_err(PublishFailure::BeforeHistory)?;
        if wire.len() > self.max_sample_size {
            return Err(PublishFailure::BeforeHistory(
                TransportError::SampleTooLarge {
                    size: wire.len(),
                    limit: self.max_sample_size,
                },
            ));
        }
        let sample = self.publisher.loan_slice_uninit(wire.len()).map_err(|e| {
            PublishFailure::BeforeHistory(TransportError::Backend(format!(
                "iceoryx2 loan_slice_uninit: {e:?}"
            )))
        })?;
        let sample = sample.write_from_slice(&wire);
        let recipients = sample.send().map_err(classify_send_error)?;
        self.consumed = true;
        Ok(recipients)
    }

    /// §2.2 wire 格式合成 —— 与 `mock::build_sample` 同算法（两遍 probe+real
    /// `FrameHeader` 保证 vtable 长度稳定，详见 mock.rs 注释与
    /// `memory/flatc_25_pitfalls.md` 坑 4）。重复实现而非抽公用是因为 mock
    /// 内部 `schema_id` 字段绑定、本路径 `schema_version` 字段绑定；Step 5
    /// 之后若抽公用可考虑 `wire::encode` 独立 mod。
    fn build_sample(&self) -> Result<Vec<u8>, TransportError> {
        const PLACEHOLDER_NONZERO: u32 = 0x0101_0101;

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

        let probe_blob_offset = if self.blob_bytes.is_some() {
            PLACEHOLDER_NONZERO
        } else {
            0
        };

        // probe header：body/blob offset 用 placeholder 探测 FrameHeader
        // 占用长度 H；后续用 H 计算真实 body_offset = 4 + H。两遍 build 的
        // 所有非 offset 字段（stamp_ns / seq / flags 等）保持完全一致，
        // 使 vtable 长度在两遍间稳定。
        let probe_header = encode_header(&FrameHeaderArgs {
            magic: 0x5242_5443,
            abi_version: 1,
            schema_id: self.schema_id,
            schema_version: self.schema_version,
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
            schema_version: self.schema_version,
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

impl Drop for Iceoryx2SampleWriter<'_> {
    fn drop(&mut self) {
        // 未 publish：本实现走"延迟 loan"，drop 前未向 iceoryx2 申请 shm，
        // 因此无归还动作。仅 buffer 随 drop 释放。
        // 对照 iceoryx2 原生 SampleMut：drop 时归还 shm slot；我们把归还时
        // 点前移到了 loan_slice_uninit 成功之后（publish 路径内部），因此
        // 本 Drop impl 不需要 touch publisher。
    }
}

fn encode_header(args: &FrameHeaderArgs) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();
    let fh = FbFrameHeader::create(&mut fbb, args);
    fbb.finish(fh, None);
    fbb.finished_data().to_vec()
}

/// 仅依据 0.9.3 的真实 `send_sample` 顺序分类，不根据错误文字猜测副作用。
fn classify_send_error(error: iceoryx2::port::SendError) -> PublishFailure {
    use iceoryx2::port::SendError;
    let source = TransportError::Backend(format!("iceoryx2 send: {error:?}"));
    match error {
        SendError::ConnectionBrokenSinceSenderNoLongerExists | SendError::ConnectionError(_) => {
            PublishFailure::BeforeHistory(source)
        }
        SendError::UnableToDeliver
        | SendError::InternalError
        | SendError::ConnectionCorrupted
        | SendError::LoanError(_) => PublishFailure::Uncertain(source),
    }
}

#[cfg(test)]
mod publish_failure_tests {
    use super::*;
    use iceoryx2::port::update_connections::ConnectionFailure;
    use iceoryx2::port::{LoanError, SendError};
    use iceoryx2_cal::zero_copy_connection::ZeroCopyCreationError;

    #[test]
    fn all_send_error_variants_use_the_real_history_boundary() {
        // 这是同一生产映射函数的枚举夹具，不宣称真实内核触发过这些故障。
        for (error, before) in [
            (SendError::ConnectionBrokenSinceSenderNoLongerExists, true),
            (
                SendError::ConnectionError(ConnectionFailure::FailedToEstablishConnection(
                    ZeroCopyCreationError::InternalError,
                )),
                true,
            ),
            (SendError::UnableToDeliver, false),
            (SendError::InternalError, false),
            (SendError::ConnectionCorrupted, false),
            (SendError::LoanError(LoanError::OutOfMemory), false),
        ] {
            let original = TransportError::Backend(format!("iceoryx2 send: {error:?}"));
            let failure = classify_send_error(error);
            assert_eq!(matches!(&failure, PublishFailure::BeforeHistory(_)), before);
            assert_eq!(failure.into_transport(), original);
        }
    }
}
