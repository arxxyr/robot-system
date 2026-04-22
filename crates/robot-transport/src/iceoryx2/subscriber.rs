//! `Iceoryx2Subscriber<T>` + `Iceoryx2SampleLease` —— 真 iceoryx2 后端
//! 的订阅侧。
//!
//! iceoryx2 `Subscriber::receive()` 返回 `Option<Sample<Service, [u8], ()>>`，
//! 其中 `Sample` 按 **owned Arc 共享状态**持有 shm slot —— 可跨线程移动，生命
//! 期独立于 Subscriber（subscriber 可以在 sample 之前 drop）。我们把 `Sample`
//! 直接塞进 `Iceoryx2SampleLease`，drop 时 iceoryx2 自动归还 shm。
//!
//! `latest()` 语义（Step 4 `LatestSubscriber` trait 约定）：取**最新一条**
//! sample；历史样本按 iceoryx2 subscriber buffer drain，这里实现为 "拿到
//! buffer 里所有 sample 后保留最后一条"，其他 drop。Step 5 MVP 做法，未来
//! 若需要 keep-all / history 语义再扩 trait。

use core::marker::PhantomData;

use iceoryx2::port::subscriber::Subscriber as IxSubscriber;
use iceoryx2::sample::Sample as IxSample;
use iceoryx2::service::ipc::Service as IxService;
use robot_msg::markers::BodyMarker;

use crate::error::TransportError;

pub(crate) type IxPubSubSubscriber = IxSubscriber<IxService, [u8], ()>;
type IxPubSubSample = IxSample<IxService, [u8], ()>;

/// 真后端订阅者。
pub struct Iceoryx2Subscriber<T: BodyMarker> {
    pub(crate) inner: IxPubSubSubscriber,
    pub(crate) _ty: PhantomData<T>,
}

impl<T: BodyMarker> core::fmt::Debug for Iceoryx2Subscriber<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Iceoryx2Subscriber")
            .field("marker", &T::SCHEMA_NAME)
            .finish()
    }
}

impl<T: BodyMarker> Iceoryx2Subscriber<T> {
    /// 取最新可用 sample；drain iceoryx2 buffer 至只剩一条。
    ///
    /// 错误路径只来自 iceoryx2 `receive()` 本身 → `TransportError::Backend`。
    pub(crate) fn iceoryx2_latest(
        &mut self,
    ) -> Result<Option<Iceoryx2SampleLease>, TransportError> {
        let mut latest: Option<IxPubSubSample> = None;
        loop {
            match self.inner.receive() {
                Ok(Some(sample)) => {
                    latest = Some(sample);
                }
                Ok(None) => break,
                Err(e) => {
                    return Err(TransportError::Backend(format!("iceoryx2 receive: {e:?}")));
                }
            }
        }
        Ok(latest.map(|sample| Iceoryx2SampleLease { sample }))
    }
}

/// 接收端 sample 持有者；drop 时 iceoryx2 自动归还 shm slot。
pub struct Iceoryx2SampleLease {
    sample: IxPubSubSample,
}

impl core::fmt::Debug for Iceoryx2SampleLease {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Iceoryx2SampleLease")
            .field("payload_len", &self.sample.payload().len())
            .finish()
    }
}

impl Iceoryx2SampleLease {
    /// 从 sample 借 payload 字节供 parser 验证。生命周期绑 `&self`。
    #[inline]
    pub(crate) fn bytes(&self) -> &[u8] {
        self.sample.payload()
    }
}
