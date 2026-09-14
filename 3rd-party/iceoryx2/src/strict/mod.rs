// RR R2（robot-system 本地补丁，2026-09-14）：受检的严格单 server / 唯一回复 RR 入口。
//
// 承接已 ACK 的 r2（IOX 函数合同）与 r3 §一（最后 shared 退出）。普通多目标 / 多回复 API 保持
// 原语义；本模块只提供成对的受检入口：`StrictClient`（单 server、Static、DiscardData、无自定义
// handler、请求不 overflow）与 `StrictServer`（单 server、Static、DiscardData、无自定义 handler）。
//
// 所有权规则概要：
// - strict 请求序号独立计数、不回绕：合法域 1..=2^62−1，只有下一次才 typed 耗尽，拒绝不消耗 loan / channel；
// - 唯一 enqueue 之前先预留本 channel 固定槽并 pin 原 route，再 CAL `strict_prepare`；发布后只做
//   不可失败的提交；`NotEnqueued` 按原件撤销预留 / prepare 并完整返回原 request；
// - `StrictPendingResponse` 私有持有真实回执的 issue（只读 getter），Drop 时把原件按值转入固定
//   Detached 槽并只发原 hint；正常维护由 `ClientSharedState::maintain_detached` 执行；
// - `StrictActiveRequest` 按值进入唯一 response loan；发送或丢弃并释放本地 loan 之后才 finish 原 Active。
//
// CAL 的 `strict_channel_status` 只是观测原语：同值 CLOSED 分不清初始 / 复用代次，本模块由成功
// prepare、完整原端点身份、原 route / pin 与未归还的 channel 建立调用资格；不使用自报 bool、
// 伪 generation 原子或地址身份。
//
// 许可：与所在 crate 相同（MIT OR Apache-2.0）。

/// 受检的 `StrictClient` 及其请求 / Pending 类型。
pub mod client;
/// 受检的 `StrictServer` 及其 Active / 唯一回复类型。
pub mod server;
#[cfg(test)]
mod tests;

pub use client::{
    StrictClient, StrictNotSent, StrictPendingResponse, StrictRequestMut, StrictRequestMutUninit,
};
pub use server::{
    StrictActiveRequest, StrictLoanRefused, StrictResponseMut, StrictResponseMutUninit, StrictSent,
    StrictServer,
};

// RR R3：公开现有 strict 返回类型，调用方无需直接依赖 CAL；类型与状态机不变。
pub use iceoryx2_cal::zero_copy_connection::{EnqueueIssue, StrictChannelStatus};

#[cfg(feature = "r2-test-fixtures")]
#[doc(hidden)]
pub use client::test_support;

use iceoryx2_cal::zero_copy_connection::{StrictPrepareError, ZeroCopySendError};

use crate::port::{LoanError, ReceiveError, update_connections::ConnectionFailure};
use crate::service::port_factory::{client::ClientCreateError, server::ServerCreateError};

/// strict 入口的配置拒绝；每个不合法项单独报告，不以默认值代替。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrictCreateError {
    /// 服务静态配置 `max_servers` 不为 1。
    MaxServersNotOne {
        /// 服务静态配置实际允许的 server 数。
        actual: usize,
    },
    /// 端口分配策略不是 `AllocationStrategy::Static`。
    AllocationNotStatic,
    /// 背压策略不是 `BackpressureStrategy::DiscardData`。
    BackpressureNotDiscardData,
    /// 设置了自定义 backpressure handler。
    CustomBackpressureHandler,
    /// 服务静态配置允许请求 overflow。
    RequestOverflowEnabled,
    /// 底层 Client 创建失败。
    Client(ClientCreateError),
    /// 底层 Server 创建失败。
    Server(ServerCreateError),
}

impl core::fmt::Display for StrictCreateError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "StrictCreateError::{self:?}")
    }
}

impl core::error::Error for StrictCreateError {}

/// strict loan 的 typed 拒绝；拒绝不消耗 loan / channel / 请求序号。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrictLoanError {
    /// 请求序号已用尽（最后合法值 2^62−1 已分配），不回绕。
    RequestIdExhausted,
    /// 没有空闲 response channel（全部被 Live / Detached 请求占用）。
    NoChannelAvailable,
    /// 底层 loan 失败。
    Loan(LoanError),
}

impl core::fmt::Display for StrictLoanError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "StrictLoanError::{self:?}")
    }
}

impl core::error::Error for StrictLoanError {}

/// strict 发送在唯一 enqueue 之前被拒绝的原因（此时无远端接纳，原 request 完整返回）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrictSendRefusal {
    /// 连接更新失败：禁止发送。
    UpdateConnections(ConnectionFailure),
    /// 已达 `max_active_requests_per_client`。
    ExceedsMaxActiveRequests,
    /// 注册表中没有 server。
    NoServer,
    /// 注册表中不止一个 server（strict 要求恰一个）。
    MultipleServers {
        /// 注册表中当前的 server 数。
        count: usize,
    },
    /// 注册表中的 server 不是 Static 数据段。
    ServerNotStatic,
    /// 发送连接与响应连接没有同时精确指向该 server。
    TargetMismatch,
    /// 本 channel 的固定槽不是 Free（内部不变量损坏）。
    SlotBusy,
    /// CAL strict prepare 失败（当前不是 CLOSED 或序号非法）。
    Prepare(StrictPrepareError),
    /// 真实 push 内核在 publication 前拒绝。
    NotEnqueued(ZeroCopySendError),
}

/// strict 回复发送失败；无论哪种失败，本地 loan 仍按原件释放，之后才 finish 原 Active。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrictResponseSendError {
    /// 连接更新失败。
    UpdateConnections(ConnectionFailure),
    /// 完整原 client id 已不在任何连接。
    TargetGone,
    /// 提示 index 上现在是另一个 client，且原 id 也不在其它 index。
    TargetChanged,
    /// 真实 push 内核在 publication 前拒绝（例如 client 响应缓冲已满）。
    NotEnqueued(ZeroCopySendError),
}

impl core::fmt::Display for StrictResponseSendError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "StrictResponseSendError::{self:?}")
    }
}

impl core::error::Error for StrictResponseSendError {}

/// strict server 接收失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrictReceiveError {
    /// 真实接收连接身份与 header client id 不一致；该请求已归还原连接。
    OriginMismatch {
        /// 真实接收连接的 sender（client）id。
        origin: u128,
        /// 请求 header 自报的 client id。
        header_client: u128,
    },
    /// 底层接收失败。
    Receive(ReceiveError),
}

impl core::fmt::Display for StrictReceiveError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "StrictReceiveError::{self:?}")
    }
}

impl core::error::Error for StrictReceiveError {}
