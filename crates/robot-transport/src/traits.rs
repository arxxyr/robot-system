//! Transport trait 抽象（`docs/00_design.md` §4.3）。
//!
//! Step 4 落地 [`LatestSubscriber`] / [`Publisher`] 的 trait + mock 实现；
//! [`RequestClient`] trait 仅声明，mock / iceoryx2 实现推到 Step 5（iceoryx2
//! request-response）+ 后续 RPC gateway 集成。
//!
//! ### 调用约定
//!
//! ```ignore
//! use robot_transport::{LatestSubscriber, Publisher};
//! use robot_transport::mock::MockChannel;
//! use robot_msg::markers::JointState;
//!
//! let ch = MockChannel::<JointState>::new();
//! let mut pub_ = ch.publisher();
//! let mut sub = ch.subscriber();
//!
//! // 发布
//! let mut w = pub_.loan()?;
//! w.set_body_bytes(&fbb_bytes);     // typed body 由调用方用 flatbuffers::FlatBufferBuilder 构造
//! w.publish()?;                      // consume writer，释放 publisher borrow
//!
//! // 订阅
//! if let Some(lease) = sub.latest()? {
//!     let view = lease.view()?;
//!     let js: robot_msg::msg::JointState<'_> = view.body.get();
//!     // ... 读 js.layout_id() / js.position_rad() ...
//! }
//! ```

use robot_core::time::Deadline;
use robot_msg::markers::BodyMarker;

use crate::error::TransportError;
use crate::sample::SampleLease;
use crate::writer::SampleWriter;

/// 最新值订阅器（`keep-last, depth=1` 语义）。
///
/// 实现者：[`crate::mock::MockSubscriber`]（Step 4）；Step 5 会加
/// `Iceoryx2Subscriber`。
pub trait LatestSubscriber<T: BodyMarker> {
    /// 返回最新 sample 的 lease；无 sample 返回 `Ok(None)`。
    fn latest(&mut self) -> Result<Option<SampleLease<T>>, TransportError>;
}

/// Loan-based 发布者。
///
/// `publish` 方法**不在本 trait 上** —— 挂在 [`SampleWriter::publish`]（按值
/// 消费 `self`）。原因见 `docs/CHANGELOG.md` 2026-04-22 "§4.3 writer API 调整"。
pub trait Publisher<T: BodyMarker> {
    /// Loan 一个可写 sample；writer 持 `&'_ mut self` 的实际借用（通过
    /// `SampleWriter::_borrow: &'_ mut u8`），writer drop 或 `publish()`
    /// 消费后才释放。
    fn loan(&mut self) -> Result<SampleWriter<'_, T>, TransportError>;
}

/// Request-response 客户端（§5 RPC / §7.3 `ConfigIdentity` 交换等同步路径用）。
///
/// Step 4 仅占位；mock 实现推到 Step 5（iceoryx2 request-response 后端就位后一起）。
pub trait RequestClient<Req, Resp> {
    fn call(&mut self, req: Req, deadline: Deadline) -> Result<Resp, TransportError>;
}

// ---------------------------------------------------------------------------
// Mock 实现
// ---------------------------------------------------------------------------

#[cfg(feature = "mock")]
impl<T: BodyMarker> Publisher<T> for crate::mock::MockPublisher<T> {
    fn loan(&mut self) -> Result<SampleWriter<'_, T>, TransportError> {
        let mock = self.mock_loan()?;
        // writer 持有 `&mut self.loan_tok` 的实际借用（非 ZST u8），
        // 编译期阻止 writer 存活期间再 `loan()`（见 tests/ui/second_loan_before_publish.rs）。
        let borrow = self.loan_borrow();
        Ok(SampleWriter::from_mock(mock, borrow))
    }
}

#[cfg(feature = "mock")]
impl<T: BodyMarker> LatestSubscriber<T> for crate::mock::MockSubscriber<T> {
    fn latest(&mut self) -> Result<Option<SampleLease<T>>, TransportError> {
        match self.mock_latest()? {
            Some(m) => Ok(Some(SampleLease::from_mock(m))),
            None => Ok(None),
        }
    }
}
