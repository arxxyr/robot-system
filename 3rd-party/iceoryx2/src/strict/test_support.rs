// RR R3：默认关闭的本方迁移故障桥。只操作原端点及原 loan，不复制发送状态机。
// 许可：与所在 crate 相同（MIT OR Apache-2.0）。

use super::*;
use crate::service::ipc;
use iceoryx2_cal::zero_copy_connection::{ZeroCopyPortDetails, ZeroCopySender};

type SliceClient = StrictClient<ipc::Service, [u8], (), [u8], ()>;
type SliceRequest = StrictRequestMut<ipc::Service, [u8], (), [u8], ()>;
type SliceShared = ClientSharedState<ipc::Service>;

/// 真实 CAL 故障路径的选择；不包含任何预制发送回执。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueFailure {
    /// 保持原无 overflow 队列，目标 B 真实遭遇满队列。
    NotEnqueued,
    /// 只对原 B 安装一次既有 CAL overflow 故障。
    EnqueueIssue,
}

/// 夹具边界或原操作失败；不允许把夹具错误当作协议通过。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixtureError {
    /// 实际原件、预算或初态不符合已批准合同。
    Boundary(&'static str),
    /// 原 strict loan 失败。
    Loan(StrictLoanError),
    /// 原内核操作失败。
    Kernel(&'static str),
}

impl core::fmt::Display for FixtureError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "FixtureError::{self:?}")
    }
}
impl core::error::Error for FixtureError {}

/// 仅复制数值，不持有原 owner，也不提供外部可写 offset。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueFixtureSnapshot {
    pub client_id: u128,
    pub server_id: u128,
    pub loans: usize,
    pub active: usize,
    pub detached: usize,
    pub pool_free: usize,
    pub channels: usize,
    pub samples: usize,
    pub queue_capacity: usize,
    pub auxiliary_references: u64,
    pub auxiliary_used: bool,
    pub armed: bool,
    pub hits: usize,
    pub target_seen: bool,
}

/// 只有按值消费原 sender 且实际 remove_sender 成功才返回此记录。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SenderCleanupRecord {
    pub client_id: u128,
    pub server_id: u128,
    pub removed: bool,
}

/// A 是真实未送 loan，保其本地引用；远端未知引用仅随原端点退役。
///
/// 调用方必须先停止并 join 原 server，再释放本守卫、client 和 Node。
/// Drop 只归还 A 本地 loan，绝不代替 peer 的退出或业务退休。
#[derive(Debug)]
pub struct RequestQueueFixture {
    auxiliary: SliceRequest,
    server_id: u128,
    target_seen: bool,
}

fn check(condition: bool, reason: &'static str) -> Result<(), FixtureError> {
    match condition {
        true => Ok(()),
        false => Err(FixtureError::Boundary(reason)),
    }
}

fn original_server(shared: &SliceShared) -> Result<(usize, u128), FixtureError> {
    let mut count = 0;
    let mut found = None;
    unsafe { &*shared.server_list_state.get() }.for_each(|index, details| {
        count += 1;
        found = Some((index, details.server_id.value(), details.data_segment_type));
        CallbackProgression::Continue
    });
    let (hint, id, kind) = found.ok_or(FixtureError::Boundary("没有原 server"))?;
    check(
        count == 1 && kind == DataSegmentType::Static,
        "须恰一个原 Static server",
    )?;
    let index = shared
        .request_sender
        .resolve_connection(hint, id)
        .ok_or(FixtureError::Boundary("原 server 请求连接不匹配"))?;
    let receiver = &shared.response_receiver;
    let key = unsafe {
        *receiver
            .connections
            .get(hint)
            .ok_or(FixtureError::Boundary("原响应连接 index 缺失"))?
            .get()
    }
    .ok_or(FixtureError::Boundary("原响应连接 key 缺失"))?;
    let connection = unsafe { &*receiver.connection_storage.get() }
        .get(key)
        .ok_or(FixtureError::Boundary("原响应连接已缺失"))?;
    check(
        connection.sender_port_id == id && connection.receiver.is_connected(),
        "原 server 响应连接不匹配",
    )?;
    Ok((index, id))
}

impl RequestQueueFixture {
    /// 在空原 client 上把已初始化且非空的 A 向原请求队列真实投递一次。
    pub fn seed(client: &SliceClient, bytes: &[u8]) -> Result<Self, FixtureError> {
        check(!bytes.is_empty(), "辅助 A 载荷必须非空")?;
        {
            let shared = client.client.client_shared_state.lock();
            let sender = &shared.request_sender;
            let config = sender.service_state.static_config().request_response();
            check(
                config.max_active_requests_per_client() == 4
                    && config.max_clients() == 8
                    && config.max_servers() == 1
                    && config.max_loaned_requests() == 2
                    && config.does_support_fire_and_forget_requests()
                    && !config.has_safe_overflow_for_requests(),
                "须原 L4/C8/M2/单 server/FF/无请求 overflow",
            )?;
            check(
                sender.number_of_samples == 10
                    && shared.strict_slots().len() == 10
                    && sender.receiver_max_buffer_size == 4
                    && sender.sender_max_borrowed_samples == 2,
                "须原 N10/queue4/loan2",
            )?;
            check(
                sender.loan_counter.load(Ordering::Relaxed) == 0
                    && shared.active_request_counter.load(Ordering::Relaxed) == 0
                    && shared
                        .strict_slots()
                        .iter()
                        .all(|slot| matches!(slot, StrictSlot::Free))
                    && unsafe { &*shared.available_channel_ids.get() }.len() == 10,
                "seed 须原 loans0/active0/Detached0/全空 channel pool",
            )?;
            // 所有样本的原引用为零，排除已有待收或被借出的辅助件。
            for state in &sender.segment_states {
                if state.payload_size() != 0 {
                    for index in 0..sender.number_of_samples {
                        check(
                            state.reference_count(index * state.payload_size()) == 0,
                            "seed 须原样本池全空闲",
                        )?;
                    }
                }
            }
            shared
                .update_connections()
                .map_err(|_| FixtureError::Kernel("建立原连接失败"))?;
            original_server(&shared)?;
        }
        let auxiliary = client
            .loan_slice_uninit(bytes.len())
            .map_err(FixtureError::Loan)?
            .write_from_slice(bytes);
        let parts = auxiliary
            .parts
            .ok_or(FixtureError::Boundary("A 原 loan 已转移"))?;
        let server_id;
        {
            let shared = auxiliary.shared.lock();
            let (index, id) = original_server(&shared)?;
            server_id = id;
            let sender = shared
                .request_sender
                .cal_sender_for_test(id)
                .ok_or(FixtureError::Boundary("缺失原 CAL sender"))?;
            check(
                sender.is_connected()
                    && sender.buffer_size() == 4
                    && sender.number_of_channels() == 1
                    && !sender.has_enabled_safe_overflow()
                    && sender.r2_fixture_armed().is_none()
                    && sender.r2_fixture_hits() == 0,
                "原 CAL queue4/连接/空注入状态不符",
            )?;
            let receipt = shared
                .request_sender
                .deliver_one_observed(
                    index,
                    id,
                    parts.offset,
                    parts.sample_size,
                    REQUEST_CHANNEL_ID,
                )
                .map_err(|_| FixtureError::Kernel("A 原投递失败"))?;
            check(
                matches!(
                    receipt,
                    TrySendReceipt::Enqueued {
                        reclaimed_overflow: None,
                        issue: None
                    }
                ),
                "A 未获得原干净入队回执",
            )?;
            check(
                sender.r2_fixture_is_used(parts.offset, REQUEST_CHANNEL_ID) == Ok(true)
                    && shared
                        .request_sender
                        .sample_reference_count_for_test(parts.offset)
                        == 2,
                "A 原 used/引用不符",
            )?;
        }
        Ok(Self {
            auxiliary,
            server_id,
            target_seen: false,
        })
    }

    /// 前三个真实请求不注入；只在原 active3/loans2 的 B 上执行一次目标操作。
    pub fn before_send(
        &mut self,
        request: &SliceRequest,
        mode: QueueFailure,
    ) -> Result<(), FixtureError> {
        let a = self
            .auxiliary
            .parts
            .ok_or(FixtureError::Boundary("A 原 loan 已转移"))?;
        let b = request
            .parts
            .ok_or(FixtureError::Boundary("B 原 loan 已转移"))?;
        check(
            request.header().client_id == self.auxiliary.header().client_id,
            "B 不属于原 client",
        )?;
        // 地址只补核原 shared 对象，同一性仍同时依完整 client/server ID 和原 loan。
        let original_shared = {
            let shared = self.auxiliary.shared.lock();
            core::ptr::from_ref(&*shared)
        };
        let shared = request.shared.lock();
        check(
            core::ptr::eq(original_shared, core::ptr::from_ref(&*shared)),
            "B 不是原 shared",
        )?;
        let active = shared.active_request_counter.load(Ordering::Relaxed);
        if active < 3 {
            return Ok(());
        }
        check(!self.target_seen, "目标 B 只能安装一次")?;
        check(
            active == 3 && shared.request_sender.loan_counter.load(Ordering::Relaxed) == 2,
            "目标 B 须原 active3/loans2",
        )?;
        check(
            a.offset != b.offset
                && a.channel_id != b.channel_id
                && b.request_id.value() == a.request_id.value() + 4,
            "B 不是三次正常发送后的不同原件",
        )?;
        check(
            shared
                .strict_slots()
                .iter()
                .filter(|slot| matches!(slot, StrictSlot::Live))
                .count()
                == 3
                && shared
                    .strict_slots()
                    .iter()
                    .all(|slot| matches!(slot, StrictSlot::Free | StrictSlot::Live))
                && unsafe { &*shared.available_channel_ids.get() }.len() == 5,
            "目标 B 的原 slot/pool 不符",
        )?;
        let (_, id) = original_server(&shared)?;
        check(id == self.server_id, "原 server 已更换")?;
        let sender = shared
            .request_sender
            .cal_sender_for_test(id)
            .ok_or(FixtureError::Boundary("原 CAL sender 已丢失"))?;
        check(
            sender.is_connected()
                && sender.buffer_size() == 4
                && sender.r2_fixture_is_used(a.offset, REQUEST_CHANNEL_ID) == Ok(true)
                && sender.r2_fixture_is_used(b.offset, REQUEST_CHANNEL_ID) == Ok(false)
                && sender.r2_fixture_armed().is_none()
                && sender.r2_fixture_hits() == 0
                && shared
                    .request_sender
                    .sample_reference_count_for_test(a.offset)
                    == 2
                && shared
                    .request_sender
                    .sample_reference_count_for_test(b.offset)
                    == 1,
            "原 A/B used/引用/armed/hits 不符",
        )?;
        self.target_seen = true;
        match mode {
            QueueFailure::NotEnqueued => Ok(()),
            QueueFailure::EnqueueIssue => {
                check(
                    sender.r2_fixture_forget_used_offset(a.offset, REQUEST_CHANNEL_ID) == Ok(true),
                    "移除原 A used 失败",
                )?;
                sender
                    .r2_fixture_arm_one_overflow_attempt(b.offset, REQUEST_CHANNEL_ID)
                    .map_err(|_| FixtureError::Kernel("安装原 B overflow 尝试失败"))?;
                check(
                    sender.r2_fixture_armed() == Some((b.offset, REQUEST_CHANNEL_ID))
                        && sender.r2_fixture_hits() == 0
                        && sender.r2_fixture_is_used(a.offset, REQUEST_CHANNEL_ID) == Ok(false),
                    "原 B armed/A used 事后核对失败",
                )
            }
        }
    }

    /// 复制真实计数；不做维护、不延长 owner 生命周期、不暴露 offset。
    pub fn snapshot(&self) -> Result<QueueFixtureSnapshot, FixtureError> {
        let a = self
            .auxiliary
            .parts
            .ok_or(FixtureError::Boundary("A 原 loan 已转移"))?;
        let shared = self.auxiliary.shared.lock();
        let sender = shared
            .request_sender
            .cal_sender_for_test(self.server_id)
            .ok_or(FixtureError::Boundary("原 CAL sender 已丢失"))?;
        Ok(QueueFixtureSnapshot {
            client_id: self.auxiliary.header().client_id.value(),
            server_id: self.server_id,
            loans: shared.request_sender.loan_counter.load(Ordering::Relaxed),
            active: shared.active_request_counter.load(Ordering::Relaxed),
            detached: shared
                .strict_slots()
                .iter()
                .filter(|slot| matches!(slot, StrictSlot::Detached(_)))
                .count(),
            pool_free: unsafe { &*shared.available_channel_ids.get() }.len(),
            channels: shared.strict_slots().len(),
            samples: shared.request_sender.number_of_samples,
            queue_capacity: sender.buffer_size(),
            auxiliary_references: shared
                .request_sender
                .sample_reference_count_for_test(a.offset),
            auxiliary_used: sender
                .r2_fixture_is_used(a.offset, REQUEST_CHANNEL_ID)
                .map_err(|_| FixtureError::Kernel("读取原 A used 失败"))?,
            armed: sender.r2_fixture_armed().is_some(),
            hits: sender.r2_fixture_hits(),
            target_seen: self.target_seen,
        })
    }
}
