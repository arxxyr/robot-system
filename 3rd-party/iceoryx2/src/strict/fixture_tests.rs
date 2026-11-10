// RR R2：默认关闭的真实故障夹具；由 tests.rs 在 r2-test-fixtures 下嵌套加载。
// 只在隔离域、原端口锁内调用真实 CAL/IOX 操作，不代写 receipt、borrow 或共享状态字。

use super::*;
use core::cell::RefCell;
use iceoryx2_cal::shm_allocator::PointerOffset;
use iceoryx2_cal::zero_copy_connection::{
    EnqueueIssue, FixtureArmError, FixtureTargetError, TrySendReceipt, ZeroCopyPortDetails,
    ZeroCopyReceiver, ZeroCopySendError, ZeroCopySender,
};

const CLEAN_ENQUEUE: TrySendReceipt = TrySendReceipt::Enqueued {
    reclaimed_overflow: None,
    issue: None,
};

/// A 是原未送 loan 的辅助样本；只有 B 走 StrictClient::send 和原 Pending 所有权转移。
#[test]
fn strict_post_push_issue_keeps_original_pending_and_auxiliary_loan() {
    let config = strict_config();
    let node = create_node(&config);
    let service = typed_builder(&node, 1, 2)
        .max_clients(1)
        .create()
        .expect("创建真实单格请求缓冲服务");
    let client = typed_strict_client(&service);
    let server = typed_strict_server(&service);
    let channels = client.number_of_channels();

    // 两份原 loan 在任何故障前分配。A 守卫从不调用 send，不被伪装成 StrictPending。
    let auxiliary = client
        .loan_uninit()
        .expect("辅助 A 原 loan")
        .write_payload(101);
    let request = client
        .loan_uninit()
        .expect("待测 B 原 loan")
        .write_payload(202);
    let auxiliary_offset = auxiliary.offset_for_test();
    let auxiliary_channel = auxiliary.channel_id();
    let original_offset = request.offset_for_test();
    let original_channel = request.channel_id();
    let original_id = request.request_id();
    assert_ne!(auxiliary_offset, original_offset);
    assert_ne!(auxiliary_channel, original_channel);
    {
        let shared = client.shared_for_test().lock();
        shared.update_connections().expect("建立原请求连接");
        let sender = shared
            .request_sender
            .cal_sender_for_test(server.id().value())
            .expect("原 CAL sender");
        assert_eq!(
            sender.r2_fixture_arm_one_overflow_attempt(original_offset, CH0),
            Err(FixtureArmError::InvalidTarget(
                FixtureTargetError::SampleSizeUnknown
            ))
        );
        let index = shared
            .request_sender
            .get_connection_id_of(server.id().value())
            .expect("原 server");
        let receipt = shared
            .request_sender
            .deliver_one_observed(
                index,
                server.id().value(),
                auxiliary_offset,
                auxiliary.sample_size_for_test(),
                CH0,
            )
            .expect("辅助 A 仅向原连接投递一次");
        assert_eq!(receipt, CLEAN_ENQUEUE);
        assert_eq!(
            shared
                .request_sender
                .sample_reference_count_for_test(auxiliary_offset),
            2
        );
        let sender = shared
            .request_sender
            .cal_sender_for_test(server.id().value())
            .expect("原 CAL sender");
        assert_eq!(sender.buffer_size(), 1);
        assert_eq!(sender.number_of_channels(), 1);
        assert!(!sender.has_enabled_safe_overflow());
        assert_eq!(sender.r2_fixture_is_used(auxiliary_offset, CH0), Ok(true));
        assert_eq!(sender.r2_fixture_armed(), None);
        assert_eq!(sender.r2_fixture_hits(), 0);
    }
    assert!(server.has_requests().expect("A 真实填满原队列，暂不消费"));

    // B 先经过真实 pin/prepare，再因 A 占满原队列而拒绝；原 B loan 完整返回。
    let refused = request.send().expect_err("真实满队列须 publication 前拒绝");
    assert_eq!(
        refused.reason,
        StrictSendRefusal::NotEnqueued(ZeroCopySendError::ReceiveBufferFull)
    );
    let request = refused.request;
    assert_eq!(request.offset_for_test(), original_offset);
    assert_eq!(request.channel_id(), original_channel);
    assert_eq!(request.request_id(), original_id);
    assert_eq!(*request.payload(), 202);
    {
        let shared = client.shared_for_test().lock();
        assert_eq!(
            client_counters(&shared),
            ClientCounters {
                loans: 2,
                active: 0,
                pool: channels - 2,
                free: channels,
                live: 0,
                detached: 0,
            }
        );
        assert_eq!(shared.response_receiver.pin_count_at_index(0), Some(0));
        assert_eq!(
            shared
                .request_sender
                .sample_reference_count_for_test(auxiliary_offset),
            2
        );
        assert_eq!(
            shared
                .request_sender
                .sample_reference_count_for_test(original_offset),
            1
        );
        let sender = shared
            .request_sender
            .cal_sender_for_test(server.id().value())
            .expect("原 CAL sender");
        assert_eq!(sender.r2_fixture_is_used(original_offset, CH0), Ok(false));
        assert_eq!(sender.r2_fixture_hits(), 0);
        assert_eq!(
            sender.r2_fixture_forget_used_offset(auxiliary_offset, CH0),
            Ok(true)
        );
        assert_eq!(sender.r2_fixture_is_used(auxiliary_offset, CH0), Ok(false));
        assert_eq!(
            sender.r2_fixture_arm_one_overflow_attempt(original_offset, CH0),
            Ok(())
        );
        for (offset, channel, error) in [
            (
                original_offset,
                ChannelId::new(1),
                FixtureTargetError::ChannelOutOfRange,
            ),
            (
                PointerOffset::from_offset_and_segment_id(
                    0,
                    iceoryx2_cal::shared_memory::SegmentId::new(1),
                ),
                CH0,
                FixtureTargetError::SegmentOutOfRange,
            ),
            (
                PointerOffset::new(original_offset.offset() + 1),
                CH0,
                FixtureTargetError::OffsetNotAligned,
            ),
            (
                PointerOffset::new(
                    auxiliary.sample_size_for_test() * shared.request_sender.number_of_chunks,
                ),
                CH0,
                FixtureTargetError::IndexOutOfRange,
            ),
        ] {
            assert_eq!(sender.r2_fixture_is_used(offset, channel), Err(error));
            assert_eq!(
                sender.r2_fixture_forget_used_offset(offset, channel),
                Err(error)
            );
            assert_eq!(
                sender.r2_fixture_arm_one_overflow_attempt(offset, channel),
                Err(FixtureArmError::InvalidTarget(error))
            );
            assert_eq!(
                sender.r2_fixture_armed(),
                Some((original_offset, CH0)),
                "非法目标不动原已装目标"
            );
        }
        assert_eq!(sender.r2_fixture_armed(), Some((original_offset, CH0)));
        assert_eq!(
            sender.r2_fixture_arm_one_overflow_attempt(original_offset, CH0),
            Err(FixtureArmError::AlreadyArmed)
        );
        assert_eq!(
            sender.r2_fixture_armed(),
            Some((original_offset, CH0)),
            "拒绝覆盖未消费目标"
        );
    }

    let pending = request
        .send()
        .expect("真实 push 后即使有 issue 仍返回原 B Pending");
    assert_eq!(
        pending.delivery_issue(),
        Some(&EnqueueIssue::OverflowedOffsetNotInUsedList(
            auxiliary_offset
        ))
    );
    assert_eq!(pending.offset_for_test(), original_offset);
    assert_eq!(pending.channel_id(), original_channel);
    assert_eq!(pending.request_id(), original_id);
    assert_eq!(*pending.payload(), 202);
    {
        let shared = client.shared_for_test().lock();
        assert_eq!(
            client_counters(&shared),
            ClientCounters {
                loans: 1,
                active: 1,
                pool: channels - 2,
                free: channels - 1,
                live: 1,
                detached: 0,
            }
        );
        assert_eq!(
            shared
                .response_receiver
                .pin_count_pinned(pending.pin_for_test()),
            1
        );
        assert_eq!(
            shared
                .request_sender
                .sample_reference_count_for_test(auxiliary_offset),
            2,
            "A 的未知引用不能冒退款"
        );
        assert_eq!(
            shared
                .request_sender
                .sample_reference_count_for_test(original_offset),
            2,
            "B 恰一次远端登记"
        );
        let sender = shared
            .request_sender
            .cal_sender_for_test(server.id().value())
            .expect("原 CAL sender");
        assert_eq!(sender.r2_fixture_armed(), None);
        assert_eq!(sender.r2_fixture_hits(), 1);
        assert_eq!(sender.r2_fixture_is_used(auxiliary_offset, CH0), Ok(false));
        assert_eq!(sender.r2_fixture_is_used(original_offset, CH0), Ok(true));
    }
    let active = receive_typed(&server);
    assert_eq!(*active.payload(), 202);
    assert_eq!(active.origin(), client.id());
    assert_eq!(active.request_id(), original_id);
    assert_eq!(active.channel_id(), original_channel);
    let response = active
        .loan_uninit()
        .expect("B 唯一回复 loan")
        .write_payload(2020);
    assert_eq!(
        response.request().offset_for_test(),
        original_offset,
        "server 原实际接收 offset"
    );
    assert_eq!(response.send(), Ok(StrictSent { issue: None }));
    assert!(
        server
            .receive()
            .expect("B Active 归还后队列已排空")
            .is_none(),
        "只收到真实挤出后的 B，未重复发布"
    );
    {
        let shared = client.shared_for_test().lock();
        shared.request_sender.retrieve_returned_chunks();
        assert_eq!(
            shared
                .request_sender
                .sample_reference_count_for_test(original_offset),
            1
        );
        assert_eq!(
            shared
                .request_sender
                .sample_reference_count_for_test(auxiliary_offset),
            2
        );
    }
    let reply = pending.receive().expect("接收 B 回复").expect("唯一回复");
    assert_eq!(*reply, 2020);
    assert_eq!(reply.origin(), server.id());
    drop(reply);
    drop(pending);
    client.maintain();
    {
        let shared = client.shared_for_test().lock();
        assert_eq!(
            client_counters(&shared),
            ClientCounters {
                loans: 1,
                active: 0,
                pool: channels - 1,
                free: channels,
                live: 0,
                detached: 0,
            }
        );
        assert_eq!(shared.response_receiver.pin_count_at_index(0), Some(0));
        assert_eq!(
            shared
                .request_sender
                .sample_reference_count_for_test(original_offset),
            0
        );
        assert_eq!(
            shared
                .request_sender
                .sample_reference_count_for_test(auxiliary_offset),
            2
        );
        let sender = shared
            .request_sender
            .cal_sender_for_test(server.id().value())
            .expect("原 CAL sender");
        assert_eq!(sender.r2_fixture_is_used(original_offset, CH0), Ok(false));
        assert_eq!(sender.r2_fixture_hits(), 1);
    }

    // B 正常闭合后退出 server，再归 A 的原本地 loan；其未知发送引用仍为 1，留原端点退出。
    drop(server);
    drop(auxiliary);
    {
        let shared = client.shared_for_test().lock();
        assert_eq!(client_counters(&shared), idle(channels));
        assert_eq!(
            shared
                .request_sender
                .sample_reference_count_for_test(auxiliary_offset),
            1,
            "不是正常退款到零"
        );
    }
    drop(client);
    assert_eq!(service.dynamic_config().number_of_clients(), 0);
    assert_eq!(service.dynamic_config().number_of_servers(), 0);
}

/// 辅助原 loan 的真实阶段。发生断言展开时，只归本地 loan；已发送/已借出等远端残件留原端点退出。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SeedPhase {
    Loaned,
    Enqueued,
    Borrowed,
    Returned,
    Reclaimed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SeedEvent {
    offset: PointerOffset,
    phase: SeedPhase,
    local_return: bool,
    references_before: u64,
    references_after: u64,
}

/// 原 allocator 的独立 loan 守卫。它不构造 Active/Response，也不补 used 或 completion 项。
struct ResponseSeed<'a> {
    server: &'a TypedStrictServer,
    offset: PointerOffset,
    sample_size: usize,
    phase: SeedPhase,
    events: &'a RefCell<Vec<SeedEvent>>,
}

impl<'a> ResponseSeed<'a> {
    fn allocate(server: &'a TypedStrictServer, events: &'a RefCell<Vec<SeedEvent>>) -> Self {
        let shared = server.shared_for_test().lock();
        let sender = &shared.response_sender;
        let chunk = sender
            .allocate(sender.chunk_layout(1))
            .expect("辅助原 response chunk");
        // chunk 成功后立即装入不分配的 RAII 守卫；后续观测失败也能归还本地 loan。
        let seed = Self {
            server,
            offset: chunk.offset,
            sample_size: chunk.size,
            phase: SeedPhase::Loaned,
            events,
        };
        let references = sender.sample_reference_count_for_test(seed.offset);
        drop(shared);
        assert_eq!(references, 1);
        seed.record(SeedPhase::Loaned, 0, references);
        seed
    }

    fn record(&self, phase: SeedPhase, before: u64, after: u64) {
        self.events.borrow_mut().push(SeedEvent {
            offset: self.offset,
            phase,
            local_return: false,
            references_before: before,
            references_after: after,
        });
    }

    fn enqueue(&mut self, client_id: u128, channel: ChannelId) {
        assert_eq!(self.phase, SeedPhase::Loaned);
        let shared = self.server.shared_for_test().lock();
        let sender = &shared.response_sender;
        let cal = sender
            .cal_sender_for_test(client_id)
            .expect("原 response CAL sender");
        let receipt = cal.try_send_observed(self.offset, self.sample_size, channel);
        // 真入队后先登记真实 IOX 发送引用并记阶段，再做断言；无中间假账窗口。
        if matches!(receipt, TrySendReceipt::Enqueued { .. }) {
            let (before, _) = sender.borrow_chunk(self.offset);
            self.phase = SeedPhase::Enqueued;
            self.record(
                self.phase,
                before,
                sender.sample_reference_count_for_test(self.offset),
            );
        }
        assert_eq!(receipt, CLEAN_ENQUEUE);
        assert_eq!(sender.sample_reference_count_for_test(self.offset), 2);
        assert_eq!(cal.r2_fixture_is_used(self.offset, channel), Ok(true));
    }

    fn receive_and_return(&mut self, client: &TypedStrictClient, pending: &TypedStrictPending) {
        assert_eq!(self.phase, SeedPhase::Enqueued);
        let shared = client.shared_for_test().lock();
        let receiver = shared
            .response_receiver
            .cal_receiver_pinned_for_test(pending.pin_for_test());
        let channel = pending.channel_id();
        assert_eq!(
            receiver.borrow_count(channel),
            1,
            "真实 B Response 正被持有"
        );
        let received = receiver.receive(channel).expect("原 CAL 接收辅助样本");
        if received.is_some() {
            self.phase = SeedPhase::Borrowed;
            let refs = self
                .server
                .shared_for_test()
                .lock()
                .response_sender
                .sample_reference_count_for_test(self.offset);
            assert_eq!(refs, 2);
            self.record(self.phase, refs, refs);
        }
        assert_eq!(received, Some(self.offset));
        assert_eq!(receiver.borrow_count(channel), 2, "辅助实际借用只暂增 1");
        receiver
            .release(self.offset, channel)
            .expect("辅助归还进入真实 completion 队列");
        self.phase = SeedPhase::Returned;
        let refs = self
            .server
            .shared_for_test()
            .lock()
            .response_sender
            .sample_reference_count_for_test(self.offset);
        assert_eq!(refs, 2);
        self.record(self.phase, refs, refs);
        assert_eq!(receiver.borrow_count(channel), 1);
        assert!(!receiver.has_data(channel));
    }

    fn reclaim(&mut self, client_id: u128, channel: ChannelId) {
        assert_eq!(self.phase, SeedPhase::Returned);
        let shared = self.server.shared_for_test().lock();
        let sender = &shared.response_sender;
        let cal = sender
            .cal_sender_for_test(client_id)
            .expect("原 response CAL sender");
        let reclaimed = cal.reclaim(channel).expect("真实 completion reclaim");
        assert_eq!(
            reclaimed,
            Some(self.offset),
            "辅助归还逐件 FIFO，不能误算失败 B"
        );
        let before = sender.sample_reference_count_for_test(self.offset);
        sender.release_chunk(self.offset);
        self.phase = SeedPhase::Reclaimed;
        let after = sender.sample_reference_count_for_test(self.offset);
        self.record(self.phase, before, after);
        assert_eq!((before, after), (2, 1));
        assert_eq!(cal.r2_fixture_is_used(self.offset, channel), Ok(false));
    }
}

impl Drop for ResponseSeed<'_> {
    fn drop(&mut self) {
        let shared = self.server.shared_for_test().lock();
        let sender = &shared.response_sender;
        let before = sender.sample_reference_count_for_test(self.offset);
        sender.return_loaned_chunk(self.offset);
        let after = sender.sample_reference_count_for_test(self.offset);
        // 保留实际退出阶段；异常时不冒 reclaim、不再次 release 远端借用。
        self.events.borrow_mut().push(SeedEvent {
            offset: self.offset,
            phase: self.phase,
            local_return: true,
            references_before: before,
            references_after: after,
        });
    }
}

/// 共用原缓冲故障流程；两个独立具名门仅改变真实 Pending/Response 释放次序。
fn run_completion_failure(pending_first: bool) {
    const COMPLETION_CAPACITY: usize = 4;
    let config = strict_config();
    let node = create_node(&config);
    let service = typed_builder(&node, 1, 1)
        .max_clients(1)
        .max_response_buffer_size(1)
        .max_borrowed_responses_per_pending_response(2)
        .create()
        .expect("buffer 1 / borrow 2 的真实服务");
    let client = typed_strict_client(&service);
    let server = service
        .server_builder()
        .backpressure_strategy(BackpressureStrategy::DiscardData)
        .max_loaned_responses_per_request(COMPLETION_CAPACITY + 1)
        .override_response_preallocation(|_| COMPLETION_CAPACITY + 1)
        .create_single_reply_strict()
        .expect("B 加四个辅助原 loan 的预算");
    let client_id = client.id();
    let channels = client.number_of_channels();
    let mut pending = Some(send_typed(&client, 303));
    let original = pending.as_ref().expect("原 Pending");
    let original_offset = original.offset_for_test();
    let original_channel = original.channel_id();
    let original_id = original.request_id();
    let active = receive_typed(&server);
    let response = active
        .loan_uninit()
        .expect("B 唯一真实 response loan")
        .write_payload(3030);
    assert_eq!(response.request().offset_for_test(), original_offset);
    let response_offset = response.offset_for_test();
    let events = RefCell::new(Vec::new());
    // 所有辅助 chunk 在填 completion 之前分配；之后不调用会自动 reclaim 的 allocate/deliver。
    let mut seeds: Vec<_> = (0..COMPLETION_CAPACITY)
        .map(|_| ResponseSeed::allocate(&server, &events))
        .collect();
    let seed_offsets: Vec<_> = seeds.iter().map(|seed| seed.offset).collect();
    for (index, offset) in seed_offsets.iter().enumerate() {
        assert_ne!(*offset, response_offset);
        assert!(!seed_offsets[..index].contains(offset));
    }
    {
        let shared = server.shared_for_test().lock();
        assert_eq!(
            shared.response_sender.loan_counter.load(Ordering::Relaxed),
            5
        );
        assert_eq!(shared.response_sender.number_of_chunks, 5);
    }
    assert_eq!(response.send(), Ok(StrictSent { issue: None }));
    let reply = original.receive().expect("真接收 B").expect("B Response");
    assert_eq!(*reply, 3030);
    assert_eq!(reply.details.offset, response_offset);
    assert_eq!(reply.channel_id, original_channel);
    assert_eq!(reply.origin(), server.id());
    assert_eq!(reply.header().request_id, original_id);
    {
        let shared = client.shared_for_test().lock();
        shared.request_sender.retrieve_returned_chunks();
        assert_eq!(
            shared
                .request_sender
                .sample_reference_count_for_test(original_offset),
            1
        );
        assert_eq!(
            shared
                .response_receiver
                .unregister_count_for_test(response_offset),
            0
        );
        let receiver = shared
            .response_receiver
            .cal_receiver_pinned_for_test(original.pin_for_test());
        assert_eq!(receiver.buffer_size(), 1);
        assert_eq!(receiver.max_borrowed_chunks(), 2);
        assert_eq!(receiver.number_of_channels(), channels);
        assert_eq!(receiver.borrow_count(original_channel), 1);
        assert_eq!(
            receiver.buffer_size() + receiver.max_borrowed_chunks() + 1,
            COMPLETION_CAPACITY
        );
    }
    for seed in &mut seeds {
        seed.enqueue(client_id.value(), original_channel);
        seed.receive_and_return(&client, original);
    }
    {
        let shared = server.shared_for_test().lock();
        assert_eq!(
            shared.response_sender.loan_counter.load(Ordering::Relaxed),
            4
        );
        assert_eq!(
            shared
                .response_sender
                .sample_reference_count_for_test(response_offset),
            1
        );
        let sender = shared
            .response_sender
            .cal_sender_for_test(client_id.value())
            .expect("原 response sender");
        assert_eq!(
            sender.r2_fixture_is_used(response_offset, original_channel),
            Ok(true)
        );
        for offset in &seed_offsets {
            assert_eq!(
                shared
                    .response_sender
                    .sample_reference_count_for_test(*offset),
                2
            );
            assert_eq!(
                sender.r2_fixture_is_used(*offset, original_channel),
                Ok(true)
            );
        }
    }

    if pending_first {
        drop(pending.take());
        client.maintain();
        let shared = client.shared_for_test().lock();
        let pin = detached_pin(&shared, original_channel);
        assert_eq!(shared.response_receiver.borrow_count_pinned(pin), 1);
        assert_eq!(
            shared
                .response_receiver
                .unregister_count_for_test(response_offset),
            0
        );
    }
    drop(reply); // 真实 unregister 成功一次，但第五次 completion.push 因 C=4 满而失败。
    if !pending_first {
        let shared = client.shared_for_test().lock();
        let original = pending
            .as_ref()
            .expect("Response 先 Drop 时原 Pending 仍活");
        assert_eq!(
            shared
                .response_receiver
                .borrow_count_pinned(original.pin_for_test()),
            1
        );
        assert_eq!(
            shared
                .response_receiver
                .unregister_count_for_test(response_offset),
            1
        );
        drop(shared);
        drop(pending.take());
    }
    for _ in 0..2 {
        client.maintain();
        let shared = client.shared_for_test().lock();
        assert_eq!(
            client_counters(&shared),
            ClientCounters {
                loans: 0,
                active: 1,
                pool: channels - 1,
                free: channels - 1,
                live: 0,
                detached: 1,
            }
        );
        let pin = detached_pin(&shared, original_channel);
        assert_eq!(
            shared.response_receiver.strict_status_pinned(pin),
            StrictChannelStatus::Closed
        );
        assert!(!shared.response_receiver.has_data_pinned(pin));
        assert_eq!(shared.response_receiver.borrow_count_pinned(pin), 1);
        assert_eq!(shared.response_receiver.pin_count_pinned(pin), 1);
        assert_eq!(
            shared
                .response_receiver
                .unregister_count_for_test(response_offset),
            1,
            "维护不重复 unregister"
        );
        assert_eq!(
            shared
                .request_sender
                .sample_reference_count_for_test(original_offset),
            1
        );
        match &shared.strict_slots()[original_channel.value()] {
            StrictSlot::Detached(detached) => {
                assert_eq!(detached.offset, original_offset);
                assert_eq!(detached.channel_id, original_channel);
                assert_eq!(detached.request_id, original_id);
            }
            _ => panic!("归还失败必须保留原 Detached 槽"),
        }
    }

    // 只 reclaim 四个真实辅助 completion 项，各自发送引用 2→1；失败 B 没有 completion 项。
    for seed in &mut seeds {
        seed.reclaim(client_id.value(), original_channel);
    }
    {
        let shared = server.shared_for_test().lock();
        let sender = shared
            .response_sender
            .cal_sender_for_test(client_id.value())
            .expect("原 response sender");
        assert_eq!(
            sender.reclaim(original_channel),
            Ok(None),
            "无重复辅助归还，也无失败 B 的伪归还"
        );
        assert_eq!(
            sender.r2_fixture_is_used(response_offset, original_channel),
            Ok(true)
        );
        assert_eq!(
            shared
                .response_sender
                .sample_reference_count_for_test(response_offset),
            1
        );
    }
    drop(seeds);
    {
        let shared = server.shared_for_test().lock();
        assert_eq!(
            shared.response_sender.loan_counter.load(Ordering::Relaxed),
            0
        );
        for offset in &seed_offsets {
            assert_eq!(
                shared
                    .response_sender
                    .sample_reference_count_for_test(*offset),
                0
            );
        }
    }
    // 逐辅助 offset 的原 allocator→真实 enqueue→borrow→completion→reclaim→本地 loan 归还账。
    for offset in &seed_offsets {
        let own: Vec<_> = events
            .borrow()
            .iter()
            .filter(|event| event.offset == *offset)
            .copied()
            .collect();
        let phases: Vec<_> = own
            .iter()
            .map(|event| (event.phase, event.local_return))
            .collect();
        assert_eq!(
            phases,
            vec![
                (SeedPhase::Loaned, false),
                (SeedPhase::Enqueued, false),
                (SeedPhase::Borrowed, false),
                (SeedPhase::Returned, false),
                (SeedPhase::Reclaimed, false),
                (SeedPhase::Reclaimed, true),
            ]
        );
        assert_eq!((own[1].references_before, own[1].references_after), (1, 2));
        assert_eq!((own[4].references_before, own[4].references_after), (2, 1));
        assert_eq!((own[5].references_before, own[5].references_after), (1, 0));
    }
    client.maintain();
    {
        let shared = client.shared_for_test().lock();
        let pin = detached_pin(&shared, original_channel);
        assert_eq!(
            shared.response_receiver.borrow_count_pinned(pin),
            1,
            "completion 有空位也不重试失败 B"
        );
        assert_eq!(shared.response_receiver.pin_count_pinned(pin), 1);
        assert_eq!(
            shared
                .response_receiver
                .unregister_count_for_test(response_offset),
            1
        );
        assert_eq!(client_counters(&shared).detached, 1);
    }

    // 失败残件只走最后 shared/Receiver 退出，再退出原 server/Sender；不计作正常回收。
    exit_trace::take();
    drop(client);
    assert_eq!(exit_trace::take(), EXIT_WITH_DETACHED);
    assert_eq!(service.dynamic_config().number_of_clients(), 0);
    {
        let shared = server.shared_for_test().lock();
        assert_eq!(
            shared
                .response_sender
                .sample_reference_count_for_test(response_offset),
            1
        );
        let sender = shared
            .response_sender
            .cal_sender_for_test(client_id.value())
            .expect("原 sender 保留到退出");
        assert!(!sender.is_connected(), "原 Receiver 已退出");
        assert_eq!(
            sender.r2_fixture_is_used(response_offset, original_channel),
            Ok(true)
        );
    }
    drop(server);
    assert_eq!(service.dynamic_config().number_of_servers(), 0);
}

#[test]
fn strict_release_failure_response_before_pending_keeps_original_borrow_and_pin() {
    run_completion_failure(false);
}

#[test]
fn strict_release_failure_pending_before_response_keeps_original_borrow_and_pin() {
    run_completion_failure(true);
}
