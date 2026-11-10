// RR R2（robot-system 本地补丁，2026-09-14）：strict 单 server / 唯一回复 RR 入口的私有具名测试。
//
// 目的：在 iceoryx2 库内以真实 SHM（`ipc_threadsafe::Service`）驱动 `StrictClient` / `StrictServer`
// 的全部所有权路径，证明 r2 §22 / r3 §三的 IOX 合同：受检成对创建与 override 后真实 N、唯一 enqueue
// 之前的 typed 拒绝完整返原 request、发布后 loan→active / sample borrow / 原 pin 各只转账一次、
// StrictPinned 保留策略下的绝对 key 与固定容量、整 server Drop 后的尾回复、完整原 client id 的
// exact 目标解析、Pending 提前 Drop 的 Detached 维护、Response 两种释放顺序、最后 shared 退出的
// 处置顺序、channel / 请求序号耗尽、以及普通双 server 的旧部分投递语义不被夹带改变。
//
// 运行（仓库根）：
//   cargo test --locked --manifest-path 3rd-party/Cargo.toml --target-dir target/rr-vendor-tests \
//       -p iceoryx2 --lib strict::
//
// 边界：
// - 每个断言都由真实 IPC 路径驱动（真实 node / service / 端口 / CAL 连接），不写固定 receipt 或
//   borrow 替身；只读的 `#[cfg(test)]` 观测钩子只用来核计数与状态，不参与判定。
// - 所有跨线程等待有界（10 s），超时 panic 文本说明卡在哪一步；不使用子进程 / SIGKILL（死亡清理门
//   由 `port/pubsub_r2_tests.rs` 覆盖），不触碰其他进程的共享内存。
// - “真实 post-push issue”与“completion 队列满导致归还失败”由默认关闭 feature 的子模块覆盖。
// - `NoChannelAvailable`、`MultipleServers`、`Prepare(..)`
//   在 strict 正常流程下由不变量排除（见各测试注释），本文件只覆盖其可达形态。
//
// 许可：与所在 crate 相同（MIT OR Apache-2.0）。

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::Cell;
use core::panic::AssertUnwindSafe;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::time::Duration;
use std::panic::catch_unwind;
use std::sync::Mutex;
use std::time::Instant;

use iceoryx2_bb_derive_macros::ZeroCopySend;
use iceoryx2_bb_elementary::allocation_strategy::AllocationStrategy;
use iceoryx2_bb_elementary_traits::zero_copy_send::ZeroCopySend;
use iceoryx2_cal::arc_sync_policy::ArcSyncPolicy;
use iceoryx2_cal::zero_copy_connection::{ChannelId, StrictChannelStatus};

use crate::config::Config;
use crate::node::{Node, NodeBuilder};
use crate::port::client::{ClientSharedState, RequestSendError, StrictSlot, exit_trace};
use crate::port::details::receiver::RoutePin;
use crate::port::update_connections::ConnectionFailure;
use crate::port::{BackpressureAction, BackpressureInfo, LoanError, ReceiveError, SendError};
use crate::prelude::{BackpressureStrategy, PortFactory};
use crate::service::builder::request_response::Builder as RrBuilder;
use crate::service::ipc_threadsafe;
use crate::service::port_factory::client::ClientCreateError;
use crate::service::port_factory::request_response::PortFactory as RrFactory;
use crate::service::port_factory::server::ServerCreateError;
use crate::strict::{
    StrictActiveRequest, StrictClient, StrictCreateError, StrictLoanError, StrictPendingResponse,
    StrictResponseSendError, StrictSendRefusal, StrictSent, StrictServer,
};
use crate::testing::{generate_isolated_config, generate_service_name};

/// 真实 SHM 的服务类型；线程安全 Arc 策略让并发 loan 门可以跨线程共享同一个 client。
type Service = ipc_threadsafe::Service;

type TypedFactory = RrFactory<Service, u64, (), u64, ()>;
type TypedStrictClient = StrictClient<Service, u64, (), u64, ()>;
type TypedStrictServer = StrictServer<Service, u64, (), u64, ()>;
type TypedStrictPending = StrictPendingResponse<Service, u64, (), u64, ()>;
type TypedStrictActive = StrictActiveRequest<Service, u64, (), u64, ()>;

type SliceFactory = RrFactory<Service, [u64], (), [u64], ()>;
type SliceStrictClient = StrictClient<Service, [u64], (), [u64], ()>;
type SliceStrictServer = StrictServer<Service, [u64], (), [u64], ()>;
type SliceStrictPending = StrictPendingResponse<Service, [u64], (), [u64], ()>;
type SliceStrictActive = StrictActiveRequest<Service, [u64], (), [u64], ()>;

const BOUND: Duration = Duration::from_secs(10);
const CH0: ChannelId = ChannelId::new(0);

#[cfg(feature = "r2-test-fixtures")]
#[path = "fixture_tests.rs"]
mod fixtures;
/// strict 请求序号的最后合法值 `2^62 − 1`。
const LAST_LEGAL_REQUEST_ID: u64 = (1u64 << 62) - 1;

/// 最后 shared 退出时仍有 Detached 槽需要处置的完整顺序。
const EXIT_WITH_DETACHED: [&str; 4] = [
    "shared_drop:dispose_detached",
    "shared_drop:detached_released",
    "shared_drop:client_handle_released",
    "shared_drop:body_end_fields_follow",
];
/// 正常回收后退出：没有任何 Detached 处置。
const EXIT_WITHOUT_DETACHED: [&str; 3] = [
    "shared_drop:dispose_detached",
    "shared_drop:client_handle_released",
    "shared_drop:body_end_fields_follow",
];

fn wait_until<F: Fn() -> bool>(what: &str, condition: F) {
    let deadline = Instant::now() + BOUND;
    while !condition() {
        assert!(Instant::now() < deadline, "等待超时（{BOUND:?}）: {what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// strict 入口要求 Static 分配与 DiscardData；typed 载荷的端口 builder 没有 `allocation_strategy`
/// setter，因此在隔离 config 上显式固定两端分配策略（不依赖上游默认值），背压策略仍由每个
/// builder 显式设置。请求 overflow 上游默认开启，这里同样显式关闭。
fn strict_config() -> Config {
    let mut config = generate_isolated_config();
    let rr = &mut config.defaults.request_response;
    rr.client_allocation_strategy = AllocationStrategy::Static;
    rr.server_allocation_strategy = AllocationStrategy::Static;
    rr.client_backpressure_strategy = BackpressureStrategy::DiscardData;
    rr.server_backpressure_strategy = BackpressureStrategy::DiscardData;
    rr.enable_safe_overflow_for_requests = false;
    config
}

fn create_node(config: &Config) -> Node<Service> {
    NodeBuilder::new()
        .config(config)
        .create::<Service>()
        .expect("创建 node")
}

/// strict 兼容的 typed 服务 builder：恰一个 server、请求不 overflow、A 个 active、L 个并行 loan。
/// override 前的 client 数据段 / channel 数 = `1 * (A + A) + L`。
fn typed_builder(
    node: &Node<Service>,
    active: usize,
    loaned: usize,
) -> RrBuilder<u64, (), u64, (), Service> {
    node.service_builder(&generate_service_name())
        .request_response::<u64, u64>()
        .max_servers(1)
        .enable_safe_overflow_for_requests(false)
        .max_active_requests_per_client(active)
        .max_loaned_requests(loaned)
}

fn typed_service(node: &Node<Service>, active: usize, loaned: usize) -> TypedFactory {
    typed_builder(node, active, loaned)
        .create()
        .expect("创建 typed 服务")
}

fn slice_service(node: &Node<Service>, active: usize, loaned: usize) -> SliceFactory {
    node.service_builder(&generate_service_name())
        .request_response::<[u64], [u64]>()
        .max_servers(1)
        .enable_safe_overflow_for_requests(false)
        .max_active_requests_per_client(active)
        .max_loaned_requests(loaned)
        .create()
        .expect("创建 slice 服务")
}

fn typed_strict_client(service: &TypedFactory) -> TypedStrictClient {
    service
        .client_builder()
        .backpressure_strategy(BackpressureStrategy::DiscardData)
        .create_single_server_strict()
        .expect("创建 strict client")
}

/// 用真实 override 把 channel / 数据段 chunk 数固定为 `channels`（上界为 worst-case）。
fn typed_strict_client_with_channels(service: &TypedFactory, channels: usize) -> TypedStrictClient {
    let client = service
        .client_builder()
        .backpressure_strategy(BackpressureStrategy::DiscardData)
        .override_request_preallocation(move |_| channels)
        .create_single_server_strict()
        .expect("创建带 override 的 strict client");
    assert_eq!(client.number_of_channels(), channels, "override 后真实 N");
    client
}

fn typed_strict_server(service: &TypedFactory) -> TypedStrictServer {
    service
        .server_builder()
        .backpressure_strategy(BackpressureStrategy::DiscardData)
        .create_single_reply_strict()
        .expect("创建 strict server")
}

fn slice_strict_client(service: &SliceFactory, max_slice_len: usize) -> SliceStrictClient {
    service
        .client_builder()
        .initial_max_slice_len(max_slice_len)
        .allocation_strategy(AllocationStrategy::Static)
        .backpressure_strategy(BackpressureStrategy::DiscardData)
        .create_single_server_strict()
        .expect("创建 slice strict client")
}

fn slice_strict_server(service: &SliceFactory, max_slice_len: usize) -> SliceStrictServer {
    service
        .server_builder()
        .initial_max_slice_len(max_slice_len)
        .allocation_strategy(AllocationStrategy::Static)
        .backpressure_strategy(BackpressureStrategy::DiscardData)
        .create_single_reply_strict()
        .expect("创建 slice strict server")
}

fn send_typed(client: &TypedStrictClient, value: u64) -> TypedStrictPending {
    client
        .loan_uninit()
        .expect("loan 请求")
        .write_payload(value)
        .send()
        .expect("唯一 enqueue 成功")
}

fn receive_typed(server: &TypedStrictServer) -> TypedStrictActive {
    server
        .receive()
        .expect("strict receive")
        .expect("应有一个受检请求")
}

fn reply_typed(active: TypedStrictActive, value: u64) -> StrictSent {
    active
        .loan_uninit()
        .expect("loan 唯一回复")
        .write_payload(value)
        .send()
        .expect("发送唯一回复")
}

fn send_slice(client: &SliceStrictClient, values: &[u64]) -> SliceStrictPending {
    client
        .loan_slice_uninit(values.len())
        .expect("loan 切片请求")
        .write_from_slice(values)
        .send()
        .expect("唯一 enqueue 成功")
}

fn receive_slice(server: &SliceStrictServer) -> SliceStrictActive {
    server
        .receive()
        .expect("strict receive")
        .expect("应有一个受检切片请求")
}

/// client 本地所有权计数的快照：loan 计数、active 计数、channel 池长度与固定槽三态。
#[derive(Debug, PartialEq, Eq)]
struct ClientCounters {
    loans: usize,
    active: usize,
    pool: usize,
    free: usize,
    live: usize,
    detached: usize,
}

fn client_counters(shared: &ClientSharedState<Service>) -> ClientCounters {
    let slots = shared.strict_slots();
    ClientCounters {
        loans: shared.request_sender.loan_counter.load(Ordering::Relaxed),
        active: shared.active_request_counter.load(Ordering::Relaxed),
        pool: unsafe { &*shared.available_channel_ids.get() }.len(),
        free: slots
            .iter()
            .filter(|slot| matches!(slot, StrictSlot::Free))
            .count(),
        live: slots
            .iter()
            .filter(|slot| matches!(slot, StrictSlot::Live))
            .count(),
        detached: slots
            .iter()
            .filter(|slot| matches!(slot, StrictSlot::Detached(_)))
            .count(),
    }
}

/// 全部 N 个 channel 空闲、无 loan / active 的期望快照。
fn idle(channels: usize) -> ClientCounters {
    ClientCounters {
        loans: 0,
        active: 0,
        pool: channels,
        free: channels,
        live: 0,
        detached: 0,
    }
}

/// 某 channel 固定槽内 Detached 原件所持的原 pin（只读借用）。
fn detached_pin(shared: &ClientSharedState<Service>, channel: ChannelId) -> &RoutePin {
    match &shared.strict_slots()[channel.value()] {
        StrictSlot::Detached(detached) => &detached.pin,
        other => panic!(
            "channel {} 的固定槽不是 Detached: {other:?}",
            channel.value()
        ),
    }
}

// ---------------------------------------------------------------------------
// 受检创建：逐项 typed 拒绝、override 只调用一次、受检容量不 panic
// ---------------------------------------------------------------------------

/// 两个原 client 各自按实际 N 建连接：N=1 与 N=3 全通道同时在途、收发、完成项归还并复用。
#[test]
fn strict_distinct_client_channel_counts_reclaim_original_connections() {
    let config = strict_config();
    let node = create_node(&config);
    let service = typed_service(&node, 3, 3);
    let small = typed_strict_client_with_channels(&service, 1);
    let large = typed_strict_client_with_channels(&service, 3);
    let server = typed_strict_server(&service);
    for round in 0..2u64 {
        let pending_small = send_typed(&small, 10 + round);
        let pending_large: Vec<_> = (0..3)
            .map(|i| send_typed(&large, 100 + round * 10 + i))
            .collect();
        let mut large_channels: Vec<_> = pending_large
            .iter()
            .map(|p| p.channel_id().value())
            .collect();
        large_channels.sort_unstable();
        assert_eq!(large_channels, [0, 1, 2]);
        for _ in 0..4 {
            let active = receive_typed(&server);
            let value = *active.payload();
            reply_typed(active, value * 10);
        }
        {
            let shared = server.shared_for_test().lock();
            for (client_id, channels) in [(small.id(), 1), (large.id(), 3)] {
                let index = shared
                    .response_sender
                    .get_connection_id_of(client_id.value())
                    .expect("原完整 client 身份");
                let connection = unsafe { &*shared.response_sender.connections[index].get() }
                    .as_ref()
                    .expect("原连接");
                assert_eq!(
                    connection.number_of_channels, channels,
                    "真实 per-client CAL channel 数"
                );
            }
        }
        let small_offset = pending_small.offset_for_test();
        assert_eq!(
            *pending_small
                .receive()
                .expect("小端 receive")
                .expect("小端回复"),
            (10 + round) * 10
        );
        let large_offsets: Vec<_> = pending_large.iter().map(|p| p.offset_for_test()).collect();
        for (i, pending) in pending_large.iter().enumerate() {
            assert_eq!(
                *pending.receive().expect("大端 receive").expect("大端回复"),
                (100 + round * 10 + i as u64) * 10
            );
        }
        drop(pending_small);
        drop(pending_large);
        for (client, offsets, channels) in
            [(&small, vec![small_offset], 1), (&large, large_offsets, 3)]
        {
            client.maintain();
            let shared = client.shared_for_test().lock();
            shared.request_sender.retrieve_returned_chunks();
            assert_eq!(client_counters(&shared), idle(channels));
            for offset in offsets {
                assert_eq!(
                    shared
                        .request_sender
                        .sample_reference_count_for_test(offset),
                    0
                );
            }
            assert_eq!(shared.response_receiver.pin_count_at_index(0), Some(0));
        }
        let shared = server.shared_for_test().lock();
        shared.response_sender.retrieve_returned_chunks();
        assert_eq!(
            shared.response_sender.loan_counter.load(Ordering::Relaxed),
            0
        );
    }
}

/// 覆盖：两入口对多 server、非 Static、非 DiscardData、自定义 handler、请求 overflow 的逐项 typed
/// 拒绝；普通 `create()` 在同一服务上保持可用。每个不合法配置只违反一项，其余合法。
#[test]
fn strict_creation_rejects_unsupported_configuration() {
    let config = strict_config();
    let node = create_node(&config);

    // max_servers != 1：两入口都拒绝；普通 API 仍可在同一服务上创建。
    {
        let service = node
            .service_builder(&generate_service_name())
            .request_response::<u64, u64>()
            .max_servers(2)
            .enable_safe_overflow_for_requests(false)
            .create()
            .expect("创建 max_servers=2 服务");
        assert_eq!(
            service
                .client_builder()
                .backpressure_strategy(BackpressureStrategy::DiscardData)
                .create_single_server_strict()
                .err(),
            Some(StrictCreateError::MaxServersNotOne { actual: 2 })
        );
        assert_eq!(
            service
                .server_builder()
                .backpressure_strategy(BackpressureStrategy::DiscardData)
                .create_single_reply_strict()
                .err(),
            Some(StrictCreateError::MaxServersNotOne { actual: 2 })
        );
        let _ordinary_client = service
            .client_builder()
            .create()
            .expect("普通 client API 保持");
        let _ordinary_server = service
            .server_builder()
            .create()
            .expect("普通 server API 保持");
        assert_eq!(service.dynamic_config().number_of_clients(), 1);
        assert_eq!(service.dynamic_config().number_of_servers(), 1);
    }

    // 非 Static 分配（切片端口 builder 的真实 setter）。
    {
        let service = slice_service(&node, 1, 1);
        assert_eq!(
            service
                .client_builder()
                .initial_max_slice_len(4)
                .allocation_strategy(AllocationStrategy::PowerOfTwo)
                .backpressure_strategy(BackpressureStrategy::DiscardData)
                .create_single_server_strict()
                .err(),
            Some(StrictCreateError::AllocationNotStatic)
        );
        assert_eq!(
            service
                .server_builder()
                .initial_max_slice_len(4)
                .allocation_strategy(AllocationStrategy::PowerOfTwo)
                .backpressure_strategy(BackpressureStrategy::DiscardData)
                .create_single_reply_strict()
                .err(),
            Some(StrictCreateError::AllocationNotStatic)
        );
    }

    // 非 DiscardData 背压 / 自定义 handler。
    {
        let service = typed_service(&node, 1, 1);
        assert_eq!(
            service
                .client_builder()
                .backpressure_strategy(BackpressureStrategy::RetryUntilDelivered)
                .create_single_server_strict()
                .err(),
            Some(StrictCreateError::BackpressureNotDiscardData)
        );
        assert_eq!(
            service
                .server_builder()
                .backpressure_strategy(BackpressureStrategy::RetryUntilDelivered)
                .create_single_reply_strict()
                .err(),
            Some(StrictCreateError::BackpressureNotDiscardData)
        );
        assert_eq!(
            service
                .client_builder()
                .backpressure_strategy(BackpressureStrategy::DiscardData)
                .set_backpressure_handler(|_: &BackpressureInfo| BackpressureAction::DiscardData)
                .create_single_server_strict()
                .err(),
            Some(StrictCreateError::CustomBackpressureHandler)
        );
        assert_eq!(
            service
                .server_builder()
                .backpressure_strategy(BackpressureStrategy::DiscardData)
                .set_backpressure_handler(|_: &BackpressureInfo| BackpressureAction::DiscardData)
                .create_single_reply_strict()
                .err(),
            Some(StrictCreateError::CustomBackpressureHandler)
        );
        // 全部合法：两入口成功，且注册表各登记一个端口。
        let client = typed_strict_client(&service);
        let server = typed_strict_server(&service);
        assert_eq!(service.dynamic_config().number_of_clients(), 1);
        assert_eq!(service.dynamic_config().number_of_servers(), 1);
        drop(client);
        drop(server);
    }

    // 请求 overflow 开启：两入口都拒绝。
    {
        let service = node
            .service_builder(&generate_service_name())
            .request_response::<u64, u64>()
            .max_servers(1)
            .enable_safe_overflow_for_requests(true)
            .create()
            .expect("创建请求 overflow 服务");
        assert_eq!(
            service
                .client_builder()
                .backpressure_strategy(BackpressureStrategy::DiscardData)
                .create_single_server_strict()
                .err(),
            Some(StrictCreateError::RequestOverflowEnabled)
        );
        assert_eq!(
            service
                .server_builder()
                .backpressure_strategy(BackpressureStrategy::DiscardData)
                .create_single_reply_strict()
                .err(),
            Some(StrictCreateError::RequestOverflowEnabled)
        );
        assert_eq!(service.dynamic_config().number_of_clients(), 0);
        assert_eq!(service.dynamic_config().number_of_servers(), 0);
    }
}

/// 覆盖：两端 override 各只调用一次、拿到的 worst-case 输入正确；override 后真实 N 统一供 channel
/// 数、固定槽、channel 池、请求数据段 chunk 数、response receiver channel 数使用；server 端 M 同理。
#[test]
fn strict_capacity_uses_single_override() {
    let config = strict_config();
    let node = create_node(&config);
    // A = 2, L = 2 → client worst-case N0 = 1 * (2 + 2) + 2 = 6；A = 2 允许两份同时在途。
    let service = typed_service(&node, 2, 2);

    let client_calls = Arc::new(AtomicUsize::new(0));
    let client_input = Arc::new(AtomicUsize::new(0));
    let client = {
        let calls = Arc::clone(&client_calls);
        let input = Arc::clone(&client_input);
        service
            .client_builder()
            .backpressure_strategy(BackpressureStrategy::DiscardData)
            .override_request_preallocation(move |worst_case| {
                calls.fetch_add(1, Ordering::Relaxed);
                input.store(worst_case, Ordering::Relaxed);
                2
            })
            .create_single_server_strict()
            .expect("创建带 override 的 strict client")
    };
    assert_eq!(
        client_calls.load(Ordering::Relaxed),
        1,
        "client override 只调用一次"
    );
    assert_eq!(
        client_input.load(Ordering::Relaxed),
        6,
        "override 输入 = worst-case N0"
    );
    assert_eq!(
        client.number_of_channels(),
        2,
        "固定槽数 = override 后真实 N"
    );
    {
        let shared = client.shared_for_test().lock();
        assert_eq!(client_counters(&shared), idle(2));
        assert_eq!(shared.response_receiver.number_of_channels, 2);
        assert_eq!(
            shared.request_sender.number_of_chunks, 2,
            "请求数据段 chunk 数 = N"
        );
    }

    // server：worst-case M0 = max_clients * (A + A) * (buffer + borrowed + k)，override 后 M = 1。
    let server_calls = Arc::new(AtomicUsize::new(0));
    let server_input = Arc::new(AtomicUsize::new(0));
    let server = {
        let calls = Arc::clone(&server_calls);
        let input = Arc::clone(&server_input);
        service
            .server_builder()
            .backpressure_strategy(BackpressureStrategy::DiscardData)
            .max_loaned_responses_per_request(1)
            .override_response_preallocation(move |worst_case| {
                calls.fetch_add(1, Ordering::Relaxed);
                input.store(worst_case, Ordering::Relaxed);
                1
            })
            .create_single_reply_strict()
            .expect("创建带 override 的 strict server")
    };
    let static_config = service.static_config();
    let expected_worst_case = static_config.max_clients()
        * (2 * static_config.max_active_requests_per_client())
        * (static_config.max_response_buffer_size()
            + static_config.max_borrowed_responses_per_pending_response()
            + 1);
    assert_eq!(
        server_calls.load(Ordering::Relaxed),
        1,
        "server override 只调用一次"
    );
    assert_eq!(server_input.load(Ordering::Relaxed), expected_worst_case);
    assert_eq!(
        server
            .shared_for_test()
            .lock()
            .response_sender
            .number_of_chunks,
        1,
        "回复数据段 chunk 数 = override 后真实 M"
    );

    // 固定 N = 2 的真实往返：两个 channel 都可用，第三个 loan 在 chunk 耗尽处 typed 拒绝（不扩槽）。
    let first = send_typed(&client, 1);
    let second = send_typed(&client, 2);
    assert_ne!(first.channel_id(), second.channel_id());
    assert_eq!(
        client.loan_uninit().err(),
        Some(StrictLoanError::Loan(LoanError::OutOfMemory)),
        "N 个 chunk 全部在途：受检拒绝而不是动态扩槽"
    );
    assert_eq!(client.number_of_channels(), 2);
    for (expected, pending) in [(1u64, &first), (2, &second)] {
        let active = receive_typed(&server);
        assert_eq!(*active.payload(), expected);
        assert_eq!(
            reply_typed(active, expected * 10),
            StrictSent { issue: None }
        );
        assert_eq!(
            *pending.receive().expect("receive").expect("原回复"),
            expected * 10
        );
    }
    drop(first);
    drop(second);
    client.maintain();
    assert_eq!(client_counters(&client.shared_for_test().lock()), idle(2));
}

/// 覆盖：受检容量算式与静态段计划在创建任何共享资源前 typed 返回，不 panic、不申请大内存；
/// `CapacityOverflow`（管理区算术溢出）与 `ExceedsMaxSupportedChunks`（池 24 位界）两端各一次。
#[test]
fn strict_creation_capacity_overflow_and_max_chunks_are_typed() {
    // (a) A = usize::MAX / 8 + 1，其余为 1：override 都返回 1，两端在连接管理区算式处受检溢出。
    {
        let mut config = strict_config();
        {
            let rr = &mut config.defaults.request_response;
            rr.max_active_requests_per_client = usize::MAX / 8 + 1;
            rr.max_response_buffer_size = 1;
            rr.max_borrowed_responses_per_pending_response = 1;
            rr.max_servers = 1;
            rr.max_clients = 1;
            rr.max_loaned_requests = 1;
            rr.server_max_loaned_responses_per_request = 1;
        }
        let node = create_node(&config);
        let service = node
            .service_builder(&generate_service_name())
            .request_response::<u64, u64>()
            .create()
            .expect("静态配置只记录数值，服务本身可创建");
        assert_eq!(
            service
                .client_builder()
                .backpressure_strategy(BackpressureStrategy::DiscardData)
                .override_request_preallocation(|_| 1)
                .create_single_server_strict()
                .err(),
            Some(StrictCreateError::Client(
                ClientCreateError::CapacityOverflow
            ))
        );
        assert_eq!(
            service
                .server_builder()
                .backpressure_strategy(BackpressureStrategy::DiscardData)
                .override_response_preallocation(|_| 1)
                .create_single_reply_strict()
                .err(),
            Some(StrictCreateError::Server(
                ServerCreateError::CapacityOverflow
            ))
        );
        assert_eq!(
            service.dynamic_config().number_of_clients(),
            0,
            "失败创建不登记端口"
        );
        assert_eq!(
            service.dynamic_config().number_of_servers(),
            0,
            "失败创建不登记端口"
        );
    }

    // (b) 池 bucket 上界：client 请求 chunk 数 N = 2 + 2^24 > 2^24 − 2。
    {
        let config = strict_config();
        let node = create_node(&config);
        let service = typed_service(&node, 1, 1 << 24);
        assert_eq!(
            service
                .client_builder()
                .backpressure_strategy(BackpressureStrategy::DiscardData)
                .create_single_server_strict()
                .err(),
            Some(StrictCreateError::Client(
                ClientCreateError::ExceedsMaxSupportedChunks
            ))
        );
        assert_eq!(service.dynamic_config().number_of_clients(), 0);

        // server 回复 chunk 数 M = 1 * (1 + 1) * (2^23 + 1 + 1) = 2^24 + 4 > 2^24 − 2。
        let service = node
            .service_builder(&generate_service_name())
            .request_response::<u64, u64>()
            .max_servers(1)
            .enable_safe_overflow_for_requests(false)
            .max_active_requests_per_client(1)
            .max_clients(1)
            .max_response_buffer_size(1 << 23)
            .max_borrowed_responses_per_pending_response(1)
            .create()
            .expect("创建大回复缓冲服务");
        assert_eq!(
            service
                .server_builder()
                .backpressure_strategy(BackpressureStrategy::DiscardData)
                .max_loaned_responses_per_request(1)
                .create_single_reply_strict()
                .err(),
            Some(StrictCreateError::Server(
                ServerCreateError::ExceedsMaxSupportedChunks
            ))
        );
        assert_eq!(service.dynamic_config().number_of_servers(), 0);

        // 同一 node 上的正常配置不受影响。
        let service = typed_service(&node, 1, 1);
        let client = typed_strict_client(&service);
        let server = typed_strict_server(&service);
        let pending = send_typed(&client, 5);
        assert_eq!(
            reply_typed(receive_typed(&server), 50),
            StrictSent { issue: None }
        );
        assert_eq!(*pending.receive().expect("receive").expect("回复"), 50);
    }
}

// ---------------------------------------------------------------------------
// 唯一 enqueue：干净入队的一次性提交与入队前的 typed 拒绝
// ---------------------------------------------------------------------------

/// 覆盖：干净入队后 loan→active 转账一次、sample 远端引用登记一次（本地 1 + 远端 1）、固定槽转
/// Live、原 pin 一次、channel 池位置由 Pending 持有；`delivery_issue()` 为 None；server 端 origin /
/// 序号 / channel 与 header 一致；唯一回复后 Closed，正常回收后原件全部释放一次。
#[test]
fn strict_clean_enqueue_commits_loan_active_and_pin_once() {
    let config = strict_config();
    let node = create_node(&config);
    let service = typed_service(&node, 2, 2);
    let client = typed_strict_client(&service);
    let server = typed_strict_server(&service);
    let channels = client.number_of_channels();
    assert_eq!(channels, 6, "N = 1 * (2 + 2) + 2");

    let request = client.loan_uninit().expect("loan");
    assert_eq!(request.header().request_id.value(), 1, "strict 序号从 1 起");
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
    }
    let request = request.write_payload(42);
    let channel = request.channel_id();
    let request_id = request.request_id();

    let pending = request.send().expect("唯一 enqueue");
    assert!(
        pending.delivery_issue().is_none(),
        "干净入队没有 post-push issue"
    );
    assert_eq!(pending.request_id(), request_id);
    assert_eq!(pending.channel_id(), channel);
    assert_eq!(*pending.payload(), 42);
    assert_eq!(pending.connection_state(), StrictChannelStatus::Open);
    assert!(!pending.has_response());
    let offset = pending.offset_for_test();
    {
        let shared = client.shared_for_test().lock();
        assert_eq!(
            client_counters(&shared),
            ClientCounters {
                loans: 0,
                active: 1,
                pool: channels - 1,
                free: channels - 1,
                live: 1,
                detached: 0,
            },
            "loan→active 转账一次，槽转 Live"
        );
        assert_eq!(
            shared
                .request_sender
                .sample_reference_count_for_test(offset),
            2,
            "本地引用 + 唯一远端登记"
        );
        let pin = pending.pin_for_test();
        assert_eq!(
            shared.response_receiver.pin_count_pinned(pin),
            1,
            "原 pin 一次"
        );
        assert_eq!(shared.response_receiver.pin_count_at_index(0), Some(1));
        assert_eq!(
            shared.response_receiver.strict_status_pinned(pin),
            StrictChannelStatus::Open
        );
        assert!(!shared.response_receiver.has_data_pinned(pin));
        assert_eq!(shared.response_receiver.borrow_count_pinned(pin), 0);
    }
    assert_eq!(client.number_of_active_requests(), 1);
    assert_eq!(client.number_of_pinned_routes(), 1);
    assert_eq!(client.number_of_detached_requests(), 0);

    let active = receive_typed(&server);
    assert_eq!(*active.payload(), 42);
    assert_eq!(active.origin(), client.id());
    assert_eq!(active.request_id(), request_id);
    assert_eq!(active.channel_id(), channel);
    assert_eq!(active.header().client_id(), client.id());
    assert_eq!(active.connection_state(), StrictChannelStatus::Open);
    assert!(!active.has_disconnect_hint());
    assert!(server.receive().expect("receive").is_none(), "只有一个请求");

    let response = active.loan_uninit().expect("loan 回复").write_payload(4200);
    assert_eq!(response.request().request_id(), request_id);
    assert_eq!(response.request().origin(), client.id());
    assert_eq!(
        server
            .shared_for_test()
            .lock()
            .response_sender
            .loan_counter
            .load(Ordering::Relaxed),
        1
    );
    assert_eq!(
        response.send().expect("唯一回复"),
        StrictSent { issue: None }
    );
    assert_eq!(
        server
            .shared_for_test()
            .lock()
            .response_sender
            .loan_counter
            .load(Ordering::Relaxed),
        0,
        "回复发送后本地 loan 释放"
    );

    assert!(pending.has_response());
    assert_eq!(pending.connection_state(), StrictChannelStatus::Closed);
    let reply = pending.receive().expect("receive").expect("唯一回复");
    assert_eq!(*reply, 4200);
    assert_eq!(reply.header().server_id(), server.id());
    assert_eq!(reply.origin(), server.id());
    assert_eq!(
        client
            .shared_for_test()
            .lock()
            .response_receiver
            .borrow_count_pinned(pending.pin_for_test()),
        1
    );
    drop(reply);
    assert!(
        pending.receive().expect("receive").is_none(),
        "唯一回复只有一份"
    );

    drop(pending);
    assert_eq!(client.number_of_detached_requests(), 1);
    client.maintain();
    {
        let shared = client.shared_for_test().lock();
        assert_eq!(client_counters(&shared), idle(channels));
        assert_eq!(
            shared.response_receiver.pin_count_at_index(0),
            Some(0),
            "原 pin 消费一次"
        );
        shared.request_sender.retrieve_returned_chunks();
        assert_eq!(
            shared
                .request_sender
                .sample_reference_count_for_test(offset),
            0,
            "本地引用与远端归还各释放一次"
        );
    }
}

/// 覆盖：唯一 enqueue 之前的 typed 拒绝——无 server、实际 peer 不是 Static、超过 active 上限——
/// 均完整返回原 request（载荷 / 序号 / channel 不变、loan 与 channel 池位置不动、固定槽不留
/// 预留），且拒绝后原 request 可直接重发。
///
/// 不可达说明：`MultipleServers` 被 `max_servers == 1` 的创建检查排除；`Prepare(..)` 需要 CLOSED
/// 之外的初值，而 channel 只在 Closed 回收后复用；`NotEnqueued(ReceiveBufferFull)` 需要 server 请求
/// 缓冲满而 active 计数未满，但缓冲容量恰等于 `max_active_requests_per_client`，active 计数总先触发
/// `ExceedsMaxActiveRequests`。`UpdateConnections(..)` 的真实触发见保留容量门。
#[test]
fn strict_pre_enqueue_refusal_preserves_original_request() {
    let config = strict_config();
    let node = create_node(&config);

    // (1) 注册表没有 server。
    let service = typed_service(&node, 1, 2);
    let client = typed_strict_client(&service);
    let channels = client.number_of_channels();
    let request = client.loan_uninit().expect("loan").write_payload(7);
    let channel = request.channel_id();
    let request_id = request.request_id();
    let refused = request.send().expect_err("无 server 必须在入队前拒绝");
    assert_eq!(refused.reason, StrictSendRefusal::NoServer);
    let request = refused.request;
    assert_eq!(*request.payload(), 7);
    assert_eq!(request.channel_id(), channel);
    assert_eq!(request.request_id(), request_id);
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
            },
            "拒绝不动 loan / channel，也不留 Reserved"
        );
        assert_eq!(
            shared.response_receiver.pin_count_at_index(0),
            None,
            "无连接可 pin"
        );
    }
    drop(request);
    assert_eq!(
        client_counters(&client.shared_for_test().lock()),
        idle(channels)
    );

    // (2) 实际选中的 peer 不是 Static：普通 Dynamic server（切片回复的真实 setter）。
    {
        let service = slice_service(&node, 1, 1);
        let dynamic_server = service
            .server_builder()
            .initial_max_slice_len(4)
            .allocation_strategy(AllocationStrategy::PowerOfTwo)
            .backpressure_strategy(BackpressureStrategy::DiscardData)
            .create()
            .expect("普通 Dynamic server");
        let client = slice_strict_client(&service, 4);
        let channels = client.number_of_channels();
        let request = client
            .loan_slice_uninit(2)
            .expect("loan")
            .write_from_slice(&[1, 2]);
        let refused = request.send().expect_err("Dynamic peer 必须拒绝");
        assert_eq!(refused.reason, StrictSendRefusal::ServerNotStatic);
        assert_eq!(refused.request.payload(), &[1, 2]);
        {
            let shared = client.shared_for_test().lock();
            let counters = client_counters(&shared);
            assert_eq!(
                (counters.loans, counters.free),
                (1, channels),
                "loan 与固定槽不动"
            );
            assert_eq!(
                shared.response_receiver.pin_count_at_index(0),
                Some(0),
                "连接已建立但未 pin"
            );
        }
        drop(refused);
        assert_eq!(
            client_counters(&client.shared_for_test().lock()),
            idle(channels)
        );
        drop(dynamic_server);
    }

    // (3) 超过 max_active_requests_per_client（A = 1，L = 2 允许第二个并行 loan）。
    let server = typed_strict_server(&service);
    let pending = send_typed(&client, 1);
    let second = client.loan_uninit().expect("第二个 loan").write_payload(2);
    let second_channel = second.channel_id();
    let second_request_id = second.request_id();
    assert_ne!(second_channel, pending.channel_id());
    let refused = second.send().expect_err("active 上限必须在入队前拒绝");
    assert_eq!(refused.reason, StrictSendRefusal::ExceedsMaxActiveRequests);
    assert_eq!(*refused.request.payload(), 2);
    assert_eq!(refused.request.channel_id(), second_channel);
    assert_eq!(refused.request.request_id(), second_request_id);
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
            shared.response_receiver.pin_count_at_index(0),
            Some(1),
            "只有第一份 pin"
        );
    }
    // 第一份正常完成（无回复 finish）并回收后，原 request 直接重发成功。
    let active = receive_typed(&server);
    assert_eq!(*active.payload(), 1);
    drop(active);
    drop(pending);
    let resent = refused.request.send().expect("第一份回收后重发");
    assert_eq!(resent.request_id(), second_request_id);
    assert_eq!(resent.channel_id(), second_channel);
    let active = receive_typed(&server);
    assert_eq!(*active.payload(), 2);
    assert_eq!(active.request_id(), second_request_id);
    assert_eq!(reply_typed(active, 20), StrictSent { issue: None });
    assert_eq!(*resent.receive().expect("receive").expect("回复"), 20);
    drop(resent);
    client.maintain();
    assert_eq!(
        client_counters(&client.shared_for_test().lock()),
        idle(channels)
    );
}

// ---------------------------------------------------------------------------
// loan 拒绝与序号：channel / chunk 耗尽、loan 额度、切片长度、最后合法序号与耗尽、并发唯一性
// ---------------------------------------------------------------------------

/// 覆盖：loan 额度 `ExceedsMaxLoans`、N 个 chunk 全在途的 `OutOfMemory`、切片长度 `ExceedsMaxLoanSize`
/// 均 typed 拒绝且不消耗 loan / channel / 序号；最后合法序号 `2^62 − 1` 可分配并真实往返；下一次才
/// `RequestIdExhausted`，不回绕，释放全部资源后仍保持耗尽。
///
/// 不可达说明：`NoChannelAvailable` 需要 channel 池先于数据段 chunk 耗尽，而两者都是 override 后的
/// 同一个 N 且由同一原件同时归还（未送 Drop / 正常维护 / 退出处置），chunk 只会更晚释放；
/// 因此可达的耗尽形态是 `Loan(OutOfMemory)`。
#[test]
fn strict_channel_and_request_id_exhaustion_preserve_ownership() {
    let config = strict_config();
    let node = create_node(&config);
    let service = typed_service(&node, 2, 2);
    let client = typed_strict_client_with_channels(&service, 2);
    let server = typed_strict_server(&service);

    // 两个并行 loan 占满 L = 2 的额度：第三个 loan 在 chunk 分配前 typed 拒绝。
    let first = client.loan_uninit().expect("loan 1");
    let second = client.loan_uninit().expect("loan 2");
    assert_eq!(first.header().request_id.value(), 1);
    assert_eq!(second.header().request_id.value(), 2);
    assert_eq!(
        client.loan_uninit().err(),
        Some(StrictLoanError::Loan(LoanError::ExceedsMaxLoans))
    );
    {
        let counters = client_counters(&client.shared_for_test().lock());
        assert_eq!((counters.loans, counters.pool, counters.free), (2, 0, 2));
    }

    // 两份都入队后 loan 额度空出，但 N = 2 个 chunk 全部在途：受检 OutOfMemory，序号不消费。
    let first = first.write_payload(1).send().expect("send 1");
    let second = second.write_payload(2).send().expect("send 2");
    assert_eq!(
        client.loan_uninit().err(),
        Some(StrictLoanError::Loan(LoanError::OutOfMemory))
    );
    assert_eq!(
        client_counters(&client.shared_for_test().lock()),
        ClientCounters {
            loans: 0,
            active: 2,
            pool: 0,
            free: 0,
            live: 2,
            detached: 0,
        }
    );

    let mut served = Vec::new();
    while let Some(active) = server.receive().expect("receive") {
        served.push(active.request_id().value());
        let value = *active.payload();
        assert_eq!(reply_typed(active, value * 10), StrictSent { issue: None });
    }
    served.sort_unstable();
    assert_eq!(served, vec![1, 2]);
    assert_eq!(*first.receive().expect("receive").expect("回复 1"), 10);
    assert_eq!(*second.receive().expect("receive").expect("回复 2"), 20);
    drop(first);
    drop(second);
    client.maintain();
    assert_eq!(client_counters(&client.shared_for_test().lock()), idle(2));

    // 被拒绝的 loan 没有消费序号：下一次成功 loan 拿到 3。
    let third = client.loan_uninit().expect("loan 3");
    assert_eq!(third.header().request_id.value(), 3);
    drop(third);
    assert_eq!(client_counters(&client.shared_for_test().lock()), idle(2));

    // 切片长度拒绝（另一服务）：在 loan 内核之前 typed 拒绝，序号与池不动。
    {
        let service = slice_service(&node, 1, 1);
        let client = slice_strict_client(&service, 4);
        let channels = client.number_of_channels();
        assert_eq!(
            client.loan_slice_uninit(5).err(),
            Some(StrictLoanError::Loan(LoanError::ExceedsMaxLoanSize))
        );
        assert_eq!(
            client_counters(&client.shared_for_test().lock()),
            idle(channels)
        );
        let request = client.loan_slice_uninit(4).expect("上限内可 loan");
        assert_eq!(request.header().request_id.value(), 1, "拒绝没有消费序号");
        drop(request);
        assert_eq!(
            client_counters(&client.shared_for_test().lock()),
            idle(channels)
        );
    }

    // 最后合法序号 2^62 − 1：可分配、CAL strict prepare 接受、server 看到同一序号并回复。
    client.set_next_request_id_for_test(LAST_LEGAL_REQUEST_ID);
    let last = client
        .loan_uninit()
        .expect("最后合法序号可 loan")
        .write_payload(9);
    assert_eq!(last.request_id().value(), LAST_LEGAL_REQUEST_ID);
    let last = last.send().expect("最后合法序号可发送");
    let active = receive_typed(&server);
    assert_eq!(active.request_id().value(), LAST_LEGAL_REQUEST_ID);
    assert_eq!(reply_typed(active, 99), StrictSent { issue: None });
    assert_eq!(*last.receive().expect("receive").expect("回复"), 99);

    // 下一次才耗尽：typed 拒绝且不动任何原件。
    assert_eq!(
        client.loan_uninit().err(),
        Some(StrictLoanError::RequestIdExhausted)
    );
    assert_eq!(
        client_counters(&client.shared_for_test().lock()),
        ClientCounters {
            loans: 0,
            active: 1,
            pool: 1,
            free: 1,
            live: 1,
            detached: 0,
        }
    );
    drop(last);
    client.maintain();
    assert_eq!(client_counters(&client.shared_for_test().lock()), idle(2));
    assert_eq!(
        client.loan_uninit().err(),
        Some(StrictLoanError::RequestIdExhausted),
        "耗尽不回绕"
    );
}

/// 覆盖（early_design_decision 并发序号项）：线程安全 Service 上多线程同时 loan 并在全部完成前
/// 持有各自的 loan，序号检查与消费在同一 shared 锁事务内，拿到的序号两两不同且恰为 1..=T。
#[test]
fn strict_concurrent_loans_take_unique_request_ids() {
    const THREADS: usize = 4;
    let config = strict_config();
    let node = create_node(&config);
    // A = 4、L = 4：N = 12 个 channel，允许 4 个并行 loan。
    let service = typed_service(&node, 4, THREADS);
    let client = typed_strict_client(&service);
    let channels = client.number_of_channels();

    let loaned = AtomicUsize::new(0);
    let ids = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..THREADS {
            scope.spawn(|| {
                let request = client
                    .loan_uninit()
                    .expect("并发 loan 在额度与 channel 充足时必须成功");
                ids.lock()
                    .expect("ids 锁")
                    .push(request.header().request_id.value());
                loaned.fetch_add(1, Ordering::AcqRel);
                // 持有本线程的 loan 直到所有线程都完成 loan，保证 loan 期间真实重叠。
                wait_until("其它线程完成各自的 loan", || {
                    loaned.load(Ordering::Acquire) >= THREADS
                });
                drop(request);
            });
        }
    });
    let mut ids = ids.into_inner().expect("ids 锁");
    ids.sort_unstable();
    assert_eq!(
        ids,
        (1..=THREADS as u64).collect::<Vec<_>>(),
        "并发序号两两不同"
    );
    assert_eq!(
        client_counters(&client.shared_for_test().lock()),
        idle(channels)
    );
}

// ---------------------------------------------------------------------------
// 泛型初始化展开：用户 header Default 的真实 unwind 与 write_from_slice 的等长要求
// ---------------------------------------------------------------------------

/// prepare 已由另一原序号占用时，失败发送只撤本次 pin，不关闭他人已准备的原状态。
#[test]
fn strict_prepare_refusal_preserves_original_prepared_request() {
    let config = strict_config();
    let node = create_node(&config);
    let service = typed_service(&node, 1, 1);
    let client = typed_strict_client(&service);
    let channels = client.number_of_channels();
    let server = typed_strict_server(&service);
    let request = client.loan_uninit().expect("原 loan").write_payload(61);
    let channel = request.channel_id();
    let original_id = request.request_id();
    let other_id = crate::active_request::RequestId::new(600).expect("合法另一序号");
    let other_pin = {
        let shared = client.shared_for_test().lock();
        shared.update_connections().expect("建立原连接");
        let pin = shared
            .response_receiver
            .pin_connection(0, server.id().value(), channel, other_id)
            .expect("原 route pin");
        shared
            .response_receiver
            .strict_prepare_pinned(&pin)
            .expect("真实 prepare 另一原序号");
        pin
    };
    let refused = request.send().expect_err("prepare 已被占用");
    assert_eq!(
        refused.reason,
        StrictSendRefusal::Prepare(
            iceoryx2_cal::zero_copy_connection::StrictPrepareError::NotClosed(
                StrictChannelStatus::Unknown
            )
        )
    );
    assert_eq!(refused.request.request_id(), original_id);
    assert_eq!(refused.request.channel_id(), channel);
    assert_eq!(*refused.request.payload(), 61);
    {
        let shared = client.shared_for_test().lock();
        assert_eq!(
            shared.response_receiver.strict_status_pinned(&other_pin),
            StrictChannelStatus::Open,
            "失败 prepare 未 close 他人原状态"
        );
        assert_eq!(
            shared.response_receiver.pin_count_pinned(&other_pin),
            1,
            "只撤回本次预留 pin"
        );
        assert_eq!(
            client_counters(&shared),
            ClientCounters {
                loans: 1,
                active: 0,
                pool: channels - 1,
                free: channels,
                live: 0,
                detached: 0
            }
        );
        shared.response_receiver.close_channel_pinned(&other_pin); // 只撤销测试自身成功的 prepare。
        assert_eq!(
            shared.response_receiver.strict_status_pinned(&other_pin),
            StrictChannelStatus::Closed
        );
        shared.response_receiver.unpin_connection(other_pin);
    }
    assert!(server.receive().expect("receive").is_none(), "拒绝未入队");
    let pending = refused
        .request
        .send()
        .expect("原 request 在原 prepare 回滚后可正常提交");
    reply_typed(receive_typed(&server), 610);
    assert_eq!(*pending.receive().expect("receive").expect("原回复"), 610);
    drop(pending);
    client.maintain();
    assert_eq!(
        client_counters(&client.shared_for_test().lock()),
        idle(channels)
    );
}

thread_local! {
    /// 置位时 `PanickingHeader::default()` panic（只影响本线程）。
    static PANIC_IN_DEFAULT: Cell<bool> = const { Cell::new(false) };
}

/// 用户 header：`Default` 按线程局部标志 panic，用来在 loan 已建立 RAII、尚未返回给用户时真实 unwind。
#[derive(Debug, ZeroCopySend)]
#[repr(C)]
struct PanickingHeader(u64);

impl Default for PanickingHeader {
    fn default() -> Self {
        if PANIC_IN_DEFAULT.with(Cell::get) {
            panic!("PanickingHeader::default 按测试要求 panic");
        }
        Self(7)
    }
}

/// 切片两端各自真实触发 Default 和逐元素 initializer 展开，单样本池随后可再次使用。
#[test]
fn strict_slice_default_and_initializer_unwind_return_original_resources() {
    let config = strict_config();
    let node = create_node(&config);
    let service = node
        .service_builder(&generate_service_name())
        .request_response::<[u64], [u64]>()
        .request_user_header::<PanickingHeader>()
        .response_user_header::<PanickingHeader>()
        .max_servers(1)
        .enable_safe_overflow_for_requests(false)
        .max_active_requests_per_client(1)
        .max_loaned_requests(1)
        .create()
        .expect("切片双 header 服务");
    let client = service
        .client_builder()
        .initial_max_slice_len(2)
        .allocation_strategy(AllocationStrategy::Static)
        .backpressure_strategy(BackpressureStrategy::DiscardData)
        .override_request_preallocation(|_| 1)
        .create_single_server_strict()
        .expect("单样本 client");
    let server = service
        .server_builder()
        .initial_max_slice_len(2)
        .allocation_strategy(AllocationStrategy::Static)
        .backpressure_strategy(BackpressureStrategy::DiscardData)
        .override_response_preallocation(|_| 1)
        .create_single_reply_strict()
        .expect("单样本 server");

    PANIC_IN_DEFAULT.with(|flag| flag.set(true));
    let default_panic = catch_unwind(AssertUnwindSafe(|| drop(client.loan_slice_uninit(2))));
    PANIC_IN_DEFAULT.with(|flag| flag.set(false));
    assert!(default_panic.is_err());
    assert_eq!(client_counters(&client.shared_for_test().lock()), idle(1));
    let initialized = Cell::new(0);
    let request = client.loan_slice_uninit(2).expect("Default 展开后原池可用");
    let initializer_panic = catch_unwind(AssertUnwindSafe(|| {
        drop(request.write_from_fn(|index| {
            initialized.set(initialized.get() + 1);
            assert_eq!(index, 0, "第二元素真实 initializer panic");
            41
        }));
    }));
    assert!(initializer_panic.is_err());
    assert_eq!(initialized.get(), 2);
    assert_eq!(client_counters(&client.shared_for_test().lock()), idle(1));

    for default_panics in [true, false] {
        let pending = client
            .loan_slice_uninit(2)
            .expect("原请求池复用")
            .write_from_fn(|index| index as u64)
            .send()
            .expect("请求提交");
        let active = server.receive().expect("receive").expect("原 Active");
        let initialized = Cell::new(0);
        PANIC_IN_DEFAULT.with(|flag| flag.set(default_panics));
        let unwind = catch_unwind(AssertUnwindSafe(|| {
            let loan = active.loan_slice_uninit(2).expect("原回复 loan");
            drop(loan.write_from_fn(|index| {
                initialized.set(initialized.get() + 1);
                assert_eq!(index, 0, "第二元素真实 initializer panic");
                51
            }));
        }));
        PANIC_IN_DEFAULT.with(|flag| flag.set(false));
        assert!(unwind.is_err());
        assert_eq!(initialized.get(), if default_panics { 0 } else { 2 });
        assert_eq!(
            server
                .shared_for_test()
                .lock()
                .response_sender
                .loan_counter
                .load(Ordering::Relaxed),
            0
        );
        assert_eq!(pending.connection_state(), StrictChannelStatus::Closed);
        assert!(pending.receive().expect("receive").is_none());
        drop(pending);
        client.maintain();
        assert_eq!(client_counters(&client.shared_for_test().lock()), idle(1));
    }
    let pending = client
        .loan_slice_uninit(2)
        .expect("所有展开后正常 loan")
        .write_from_fn(|i| 60 + i as u64)
        .send()
        .expect("提交");
    server
        .receive()
        .expect("receive")
        .expect("原 Active")
        .loan_slice_uninit(2)
        .expect("所有展开后回复池复用")
        .write_from_fn(|i| 70 + i as u64)
        .send()
        .expect("唯一回复");
    assert_eq!(
        pending.receive().expect("receive").expect("回复").payload(),
        &[70, 71]
    );
    drop(pending);
    client.maintain();
    assert_eq!(client_counters(&client.shared_for_test().lock()), idle(1));
}

/// 覆盖：请求侧 loan 在写用户 header Default 时 panic → 已建立的 loan RAII 归还 loan 与 channel，
/// 固定槽全部 Free；随后 client 仍可正常 loan / 发送并把 header 默认值带到 server。
#[test]
fn strict_default_unwind_returns_request_loan_and_channel() {
    let config = strict_config();
    let node = create_node(&config);
    let service = node
        .service_builder(&generate_service_name())
        .request_response::<u64, u64>()
        .request_user_header::<PanickingHeader>()
        .max_servers(1)
        .enable_safe_overflow_for_requests(false)
        .max_active_requests_per_client(1)
        .max_loaned_requests(1)
        .create()
        .expect("创建带用户 header 的服务");
    let client = service
        .client_builder()
        .backpressure_strategy(BackpressureStrategy::DiscardData)
        .create_single_server_strict()
        .expect("创建 strict client");
    let server = service
        .server_builder()
        .backpressure_strategy(BackpressureStrategy::DiscardData)
        .create_single_reply_strict()
        .expect("创建 strict server");
    let channels = client.number_of_channels();

    PANIC_IN_DEFAULT.with(|flag| flag.set(true));
    let outcome = catch_unwind(AssertUnwindSafe(|| client.loan_uninit().map(|_| ())));
    PANIC_IN_DEFAULT.with(|flag| flag.set(false));
    assert!(
        outcome.is_err(),
        "用户 header Default 的 panic 必须穿出 loan_uninit"
    );
    assert_eq!(
        client_counters(&client.shared_for_test().lock()),
        idle(channels),
        "unwind 后 loan 计数、channel 池与固定槽全部归还"
    );

    // 之后的 loan 正常：header 默认值可见。unwind 的那次 loan 已在锁内消费了序号 1（它不是 typed
    // 拒绝），因此这里观察到的序号是 2；序号只递增不回绕，跳号无害。
    let request = client
        .loan_uninit()
        .expect("unwind 后可再次 loan")
        .write_payload(3);
    assert_eq!(request.user_header().0, 7);
    assert_eq!(request.request_id().value(), 2);
    let pending = request.send().expect("send");
    let active = server.receive().expect("receive").expect("请求");
    assert_eq!(active.user_header().0, 7);
    assert_eq!(*active.payload(), 3);
    assert_eq!(
        active
            .loan_uninit()
            .expect("loan 回复")
            .write_payload(30)
            .send()
            .expect("回复"),
        StrictSent { issue: None }
    );
    assert_eq!(*pending.receive().expect("receive").expect("回复"), 30);
    drop(pending);
    client.maintain();
    assert_eq!(
        client_counters(&client.shared_for_test().lock()),
        idle(channels)
    );
}

/// 覆盖：回复侧 loan 在写用户 header Default 时 panic → 先释放本地 response loan，再由字段析构
/// finish 原 Active（client 观察到正常 Closed 且无回复）；server 之后仍可正常回复。
#[test]
fn strict_default_unwind_releases_response_loan_then_finishes_active() {
    let config = strict_config();
    let node = create_node(&config);
    let service = node
        .service_builder(&generate_service_name())
        .request_response::<u64, u64>()
        .response_user_header::<PanickingHeader>()
        .max_servers(1)
        .enable_safe_overflow_for_requests(false)
        .max_active_requests_per_client(1)
        .max_loaned_requests(1)
        .create()
        .expect("创建带回复用户 header 的服务");
    let client = service
        .client_builder()
        .backpressure_strategy(BackpressureStrategy::DiscardData)
        .create_single_server_strict()
        .expect("创建 strict client");
    let server = service
        .server_builder()
        .backpressure_strategy(BackpressureStrategy::DiscardData)
        .create_single_reply_strict()
        .expect("创建 strict server");
    let channels = client.number_of_channels();

    let pending = client
        .loan_uninit()
        .expect("loan")
        .write_payload(5)
        .send()
        .expect("send");
    let active = server.receive().expect("receive").expect("请求");
    assert_eq!(pending.connection_state(), StrictChannelStatus::Open);

    PANIC_IN_DEFAULT.with(|flag| flag.set(true));
    let outcome = catch_unwind(AssertUnwindSafe(move || drop(active.loan_uninit())));
    PANIC_IN_DEFAULT.with(|flag| flag.set(false));
    assert!(
        outcome.is_err(),
        "回复 header Default 的 panic 必须穿出 loan_uninit"
    );
    assert_eq!(
        server
            .shared_for_test()
            .lock()
            .response_sender
            .loan_counter
            .load(Ordering::Relaxed),
        0,
        "unwind 释放了本地 response loan"
    );
    assert_eq!(
        pending.connection_state(),
        StrictChannelStatus::Closed,
        "unwind 后原 Active 已正常 finish"
    );
    assert!(!pending.has_response());
    assert!(pending.receive().expect("receive").is_none());
    drop(pending);
    client.maintain();
    assert_eq!(
        client_counters(&client.shared_for_test().lock()),
        idle(channels)
    );

    // server 之后仍可正常回复，回复 header 默认值可见。
    let pending = client
        .loan_uninit()
        .expect("loan")
        .write_payload(6)
        .send()
        .expect("send");
    let active = server.receive().expect("receive").expect("请求");
    assert_eq!(
        active
            .loan_uninit()
            .expect("loan 回复")
            .write_payload(60)
            .send()
            .expect("回复"),
        StrictSent { issue: None }
    );
    let reply = pending.receive().expect("receive").expect("回复");
    assert_eq!(*reply, 60);
    assert_eq!(reply.user_header().0, 7);
}

/// 覆盖：两端 `write_from_slice` 对短 / 长输入沿 `copy_from_slice` 语义 panic，panic 时由既有 RAII
/// 归还（请求侧：loan / channel；回复侧：本地 loan 先释放、再 finish 原 Active）；等长输入成功往返。
#[test]
fn strict_write_from_slice_requires_exact_length_on_both_ends() {
    let config = strict_config();
    let node = create_node(&config);
    let service = slice_service(&node, 1, 1);
    let client = slice_strict_client(&service, 4);
    let server = slice_strict_server(&service, 4);
    let channels = client.number_of_channels();

    // 请求侧：短 / 长都 panic 并归还；等长成功。
    for wrong in [&[1u64, 2][..], &[1, 2, 3, 4][..]] {
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            let request = client.loan_slice_uninit(3).expect("loan 3 个元素");
            let _initialized = request.write_from_slice(wrong);
        }));
        assert!(outcome.is_err(), "长度 {} != 3 必须 panic", wrong.len());
        assert_eq!(
            client_counters(&client.shared_for_test().lock()),
            idle(channels)
        );
    }
    let pending = send_slice(&client, &[1, 2, 3]);
    let active = receive_slice(&server);
    assert_eq!(active.payload(), &[1, 2, 3]);

    // 回复侧：短输入 panic → 本地 loan 释放、原 Active finish（client 看到 Closed 且无回复）。
    let outcome = catch_unwind(AssertUnwindSafe(move || {
        let response = active.loan_slice_uninit(2).expect("loan 2 个元素");
        let _initialized = response.write_from_slice(&[9]);
    }));
    assert!(outcome.is_err(), "短输入必须 panic");
    assert_eq!(
        server
            .shared_for_test()
            .lock()
            .response_sender
            .loan_counter
            .load(Ordering::Relaxed),
        0
    );
    assert_eq!(pending.connection_state(), StrictChannelStatus::Closed);
    assert!(pending.receive().expect("receive").is_none());
    drop(pending);
    client.maintain();
    assert_eq!(
        client_counters(&client.shared_for_test().lock()),
        idle(channels)
    );

    // 回复侧：长输入同样 panic 并收尾。
    let pending = send_slice(&client, &[4]);
    let active = receive_slice(&server);
    let outcome = catch_unwind(AssertUnwindSafe(move || {
        let response = active.loan_slice_uninit(2).expect("loan 2 个元素");
        let _initialized = response.write_from_slice(&[9, 9, 9]);
    }));
    assert!(outcome.is_err(), "长输入必须 panic");
    assert_eq!(
        server
            .shared_for_test()
            .lock()
            .response_sender
            .loan_counter
            .load(Ordering::Relaxed),
        0
    );
    assert_eq!(pending.connection_state(), StrictChannelStatus::Closed);
    assert!(pending.receive().expect("receive").is_none());
    drop(pending);
    client.maintain();

    // 回复侧：等长成功，client 收到完整切片。
    let pending = send_slice(&client, &[5]);
    let active = receive_slice(&server);
    let sent = active
        .loan_slice_uninit(2)
        .expect("loan 2 个元素")
        .write_from_slice(&[7, 8])
        .send()
        .expect("发送切片回复");
    assert_eq!(sent, StrictSent { issue: None });
    let reply = pending.receive().expect("receive").expect("切片回复");
    assert_eq!(reply.payload(), &[7, 8]);
    drop(reply);
    drop(pending);
    client.maintain();
    assert_eq!(
        client_counters(&client.shared_for_test().lock()),
        idle(channels)
    );
}

// ---------------------------------------------------------------------------
// 普通多目标语义门：保留部分投递 Result，并验证 0.10 失败退休修复
// ---------------------------------------------------------------------------

/// 覆盖：普通 Client 对两个普通 Server，第一目标已接受、第二目标（缓冲真实填满、从不消费）经
/// `DiscardDataAndFail` 真实失败——`send_request` 返回旧的 `Err(SendError::UnableToDeliver)`，
/// 第一份仍由第一目标收到；0.10 失败路径不占用 active 配额并关闭原响应 channel。
#[test]
fn ordinary_two_servers_keep_partial_delivery_error_semantics() {
    let config = strict_config();
    let node = create_node(&config);
    let service = node
        .service_builder(&generate_service_name())
        .request_response::<u64, u64>()
        .max_servers(2)
        .enable_safe_overflow_for_requests(false)
        .max_active_requests_per_client(1)
        .create()
        .expect("创建双 server 服务");
    let server_a = service.server_builder().create().expect("普通 server A");
    let server_b = service.server_builder().create().expect("普通 server B");
    let handler_calls = Arc::new(AtomicUsize::new(0));
    let client = {
        let calls = Arc::clone(&handler_calls);
        service
            .client_builder()
            .backpressure_strategy(BackpressureStrategy::DiscardData)
            .set_backpressure_handler(move |_: &BackpressureInfo| {
                calls.fetch_add(1, Ordering::Relaxed);
                BackpressureAction::DiscardDataAndFail
            })
            .create()
            .expect("普通 client")
    };

    // 第一份：两个目标缓冲都空，旧 recipients 语义 = 2。
    let pending = client.send_copy(1).expect("第一份两目标都接受");
    assert_eq!(pending.number_of_server_connections(), 2);
    let active_a = server_a
        .receive()
        .expect("A receive")
        .expect("A 收到第一份");
    assert_eq!(*active_a, 1);
    drop(active_a);
    assert!(
        server_b.has_requests().expect("B has_requests"),
        "B 的缓冲被第一份占满"
    );
    drop(pending);
    assert_eq!(handler_calls.load(Ordering::Relaxed), 0);

    // 第二份：A 接受在前，B 真实满缓冲由 handler 判失败 → 旧 Result 为 Err(UnableToDeliver)。
    let outcome = client.send_copy(2);
    assert_eq!(
        outcome.err(),
        Some(RequestSendError::SendError(SendError::UnableToDeliver))
    );
    assert!(
        handler_calls.load(Ordering::Relaxed) >= 1,
        "B 的满缓冲真实触发了 handler"
    );
    let active_a = server_a
        .receive()
        .expect("A receive")
        .expect("A 仍可收第二份");
    assert_eq!(*active_a, 2);
    assert!(!active_a.is_connected(), "失败投递的原响应 channel 已关闭");
    drop(active_a);
    let active_b = server_b
        .receive()
        .expect("B receive")
        .expect("B 只持有第一份");
    assert_eq!(*active_b, 1);
    drop(active_b);
    assert!(server_b.receive().expect("B receive").is_none());
    // 上游 0.10：只有成功投递才计入 active；失败时没有 Pending，立即关闭原 channel。
    assert_eq!(
        client
            .client_shared_state
            .lock()
            .active_request_counter
            .load(Ordering::Relaxed),
        0
    );
    let pending = client.send_copy(3).expect("失败后可复用 active 配额");
    assert_eq!(pending.number_of_server_connections(), 2);
    for server in [&server_a, &server_b] {
        let active = server.receive().expect("receive").expect("第三份请求");
        assert_eq!(*active, 3);
    }
    drop(pending);
    assert_eq!(
        client
            .client_shared_state
            .lock()
            .active_request_counter
            .load(Ordering::Relaxed),
        0
    );
}

// ---------------------------------------------------------------------------
// StrictPinned 保留策略：两 channel 尾件、绝对 key、满表拒绝与 dirty 重试
// ---------------------------------------------------------------------------

/// 覆盖：换代后旧连接因原 pin 被保留（原 mapping、绝对 key），两个 channel 的尾件各由原 Pending 收到；
/// `client_expired_connection_buffer = 1` 时第二次换代在覆盖旧件前 typed 拒绝
/// （`RetentionCapacityExceeded`），原 pin / key / index 不变、dirty 置位；update 失败仍按原 key 收
/// 旧 server 尾件并在 None 时返回原 update Err；失败 update 禁止新发送；外持 Response（borrow）与
/// Detached 一起阻止回收；释放后 dirty 重试真实建立新连接（新连接复用 key 0，被保留者仍按绝对 key
/// 收发），之后新代正常往返，最终全部原件回收。
#[test]
fn strict_retained_routes_cover_two_channels_nonzero_keys_and_capacity() {
    let mut config = strict_config();
    config
        .defaults
        .request_response
        .client_expired_connection_buffer = 1;
    let node = create_node(&config);
    let service = typed_service(&node, 2, 2);
    let client = typed_strict_client(&service);
    let channels = client.number_of_channels();

    // 第一代：两个 channel 的请求都由 server1 回复（Active 已 finish，尾件留在各自队列）。
    let server1 = typed_strict_server(&service);
    let server1_id = server1.id();
    let pending1 = send_typed(&client, 11);
    let pending2 = send_typed(&client, 12);
    assert_ne!(pending1.channel_id(), pending2.channel_id());
    for _ in 0..2 {
        let active = receive_typed(&server1);
        let value = *active.payload();
        assert_eq!(reply_typed(active, value * 10), StrictSent { issue: None });
    }
    drop(server1);
    assert_eq!(service.dynamic_config().number_of_servers(), 0);

    // 换代：新 server 占同一 index；旧连接因 pin 保留（存储 2 = 1 + expired 1，保留表 1）。
    let server2 = typed_strict_server(&service);
    let server2_id = server2.id();
    {
        let shared = client.shared_for_test().lock();
        shared
            .update_connections()
            .expect("首次换代：存储与保留表都有位");
        assert_eq!(shared.response_receiver.retained_len(), 1);
        assert_eq!(shared.response_receiver.number_of_stored_connections(), 2);
        assert_eq!(
            shared
                .response_receiver
                .pin_count_pinned(pending1.pin_for_test()),
            2
        );
        assert_eq!(
            shared
                .response_receiver
                .pin_count_pinned(pending2.pin_for_test()),
            2
        );
        assert_eq!(
            shared.response_receiver.pin_count_at_index(0),
            Some(0),
            "新连接未被 pin"
        );
    }
    let reply1 = pending1
        .receive()
        .expect("receive")
        .expect("channel 1 的尾件");
    assert_eq!(*reply1, 110);
    assert_eq!(reply1.header().server_id(), server1_id);
    let reply2 = pending2
        .receive()
        .expect("receive")
        .expect("channel 2 的尾件");
    assert_eq!(*reply2, 120);
    assert_eq!(reply2.header().server_id(), server1_id);
    assert_eq!(pending1.connection_state(), StrictChannelStatus::Closed);
    assert_eq!(pending2.connection_state(), StrictChannelStatus::Closed);

    // pending2 完整释放；pending1 提前 Drop 但 reply1 仍被外持（borrow 1）→ 只有 pending2 被回收。
    drop(reply2);
    drop(pending2);
    drop(pending1);
    client.maintain();
    {
        let shared = client.shared_for_test().lock();
        assert_eq!(
            client_counters(&shared).detached,
            1,
            "外持 borrow 的 Detached 不回收"
        );
        assert_eq!(shared.response_receiver.retained_len(), 1, "旧连接仍被保留");
        assert_eq!(shared.response_receiver.number_of_stored_connections(), 2);
    }

    // 第二代：server2 回复一份后整个退出。
    let pending3 = send_typed(&client, 13);
    assert_ne!(
        pending3.pin_for_test().key_for_test(),
        0,
        "第二原连接使用非零绝对 key"
    );
    assert_eq!(
        reply_typed(receive_typed(&server2), 130),
        StrictSent { issue: None }
    );
    drop(server2);
    let server3 = typed_strict_server(&service);
    let server3_id = server3.id();

    // 第二次换代：存储已满（保留的旧连接 + 被 pending3 pin 的当前连接）→ 覆盖前 typed 拒绝。
    {
        let shared = client.shared_for_test().lock();
        assert_eq!(
            shared.update_connections(),
            Err(ConnectionFailure::RetentionCapacityExceeded)
        );
        assert!(
            shared.retry_update.load(Ordering::Relaxed),
            "拒绝后保 dirty"
        );
        assert_eq!(
            shared.response_receiver.pin_count_at_index(0),
            Some(1),
            "原 index 上仍是原连接"
        );
        assert_eq!(
            shared
                .response_receiver
                .pin_count_pinned(pending3.pin_for_test()),
            1
        );
        assert_eq!(shared.response_receiver.retained_len(), 1);
        assert_eq!(shared.response_receiver.number_of_stored_connections(), 2);
    }
    // update 失败仍按原 key 收 server2 的尾件；收空后才返回原 update Err。
    let reply3 = pending3
        .receive()
        .expect("update 失败不挡原 key 的收包")
        .expect("尾件");
    assert_ne!(
        pending3.pin_for_test().key_for_test(),
        0,
        "update 失败后仍收原非零绝对 key"
    );
    assert_eq!(*reply3, 130);
    assert_eq!(reply3.header().server_id(), server2_id);
    drop(reply3);
    assert!(matches!(
        pending3.receive(),
        Err(ReceiveError::ConnectionFailure(
            ConnectionFailure::RetentionCapacityExceeded
        ))
    ));
    // 失败 update 禁止新发送：原 request 完整返回。
    let refused = client
        .loan_uninit()
        .expect("loan")
        .write_payload(14)
        .send()
        .expect_err("update 失败必须拒绝发送");
    assert_eq!(
        refused.reason,
        StrictSendRefusal::UpdateConnections(ConnectionFailure::RetentionCapacityExceeded)
    );
    assert_eq!(*refused.request.payload(), 14);
    drop(refused);

    // 释放外持的 borrow → Detached 正常回收 → 旧连接按绝对 key 回收 → dirty 重试建立新连接。
    drop(reply1);
    client.maintain();
    {
        let shared = client.shared_for_test().lock();
        assert_eq!(client_counters(&shared).detached, 0);
        assert_eq!(shared.response_receiver.retained_len(), 0);
        assert_eq!(shared.response_receiver.number_of_stored_connections(), 1);
        shared.update_connections().expect("释放后 dirty 重试成功");
        assert_eq!(
            shared.response_receiver.retained_len(),
            1,
            "被 pin 的第二代连接进入保留表"
        );
        assert_eq!(shared.response_receiver.number_of_stored_connections(), 2);
        assert_eq!(
            shared.response_receiver.pin_count_at_index(0),
            Some(0),
            "index 0 上是未被 pin 的新连接（复用了已释放的 key）"
        );
        assert_eq!(
            shared
                .response_receiver
                .pin_count_pinned(pending3.pin_for_test()),
            1,
            "原 pin 仍指向保留的第二代连接，而不是 index 0 上的新连接"
        );
    }
    let pending4 = send_typed(&client, 15);
    assert_eq!(
        reply_typed(receive_typed(&server3), 150),
        StrictSent { issue: None }
    );
    let reply4 = pending4.receive().expect("receive").expect("新代回复");
    assert_eq!(*reply4, 150);
    assert_eq!(reply4.header().server_id(), server3_id);
    drop(reply4);

    drop(pending3);
    drop(pending4);
    client.maintain();
    {
        let shared = client.shared_for_test().lock();
        assert_eq!(client_counters(&shared), idle(channels));
        assert_eq!(shared.response_receiver.retained_len(), 0);
        assert_eq!(shared.response_receiver.number_of_stored_connections(), 1);
    }
}

// ---------------------------------------------------------------------------
// server 退出序列：整 server Drop 后尾回复、无回复 Drop、Active 尚活
// ---------------------------------------------------------------------------

/// 覆盖：唯一回复发出并 finish 后整个 Server Drop（注册表空、回复数据段由 client 侧映射保持），
/// 原 Pending 仍按原 pin 收到该尾回复且 Closed 是正常 finish 的结果；回收后保留连接被释放。
#[test]
fn strict_final_reply_survives_whole_server_drop() {
    let config = strict_config();
    let node = create_node(&config);
    let service = typed_service(&node, 1, 1);
    let client = typed_strict_client(&service);
    let channels = client.number_of_channels();
    let server = typed_strict_server(&service);
    let server_id = server.id();

    let pending = send_typed(&client, 21);
    assert_eq!(
        reply_typed(receive_typed(&server), 2100),
        StrictSent { issue: None }
    );
    drop(server);
    assert_eq!(service.dynamic_config().number_of_servers(), 0);

    assert_eq!(pending.connection_state(), StrictChannelStatus::Closed);
    let reply = pending
        .receive()
        .expect("receive")
        .expect("整 server Drop 后尾回复仍可收");
    assert_eq!(*reply, 2100);
    assert_eq!(reply.header().server_id(), server_id);
    assert_eq!(reply.origin(), server_id);
    {
        let shared = client.shared_for_test().lock();
        assert_eq!(
            shared.response_receiver.retained_len(),
            1,
            "无 index 的原连接因 pin 保留"
        );
        assert_eq!(
            shared
                .response_receiver
                .borrow_count_pinned(pending.pin_for_test()),
            1
        );
    }
    drop(reply);
    assert!(pending.receive().expect("receive").is_none());

    drop(pending);
    client.maintain();
    {
        let shared = client.shared_for_test().lock();
        assert_eq!(client_counters(&shared), idle(channels));
        assert_eq!(shared.response_receiver.retained_len(), 0);
        assert_eq!(shared.response_receiver.number_of_stored_connections(), 0);
    }
}

/// 覆盖：(a) 正常无回复 Drop 即 finish → Closed、无回复、可回收；(b) Server 包装先 Drop 而 Active
/// 仍存：共享状态与注册表句柄由 Active 持有，client 只看到 Open（不冒 Closed），hint 双向可见，
/// 最后回复由仍活的 Active 发出后才 finish、注销句柄，尾回复可收。
#[test]
fn strict_no_reply_and_live_active_keep_correct_state() {
    let config = strict_config();
    let node = create_node(&config);
    let service = typed_service(&node, 2, 2);
    let client = typed_strict_client(&service);
    let channels = client.number_of_channels();
    let server = typed_strict_server(&service);

    // (a) 无回复 Drop。
    let pending = send_typed(&client, 1);
    let active = receive_typed(&server);
    assert_eq!(pending.connection_state(), StrictChannelStatus::Open);
    drop(active);
    assert_eq!(pending.connection_state(), StrictChannelStatus::Closed);
    assert!(!pending.has_response());
    assert!(pending.receive().expect("receive").is_none());
    drop(pending);
    client.maintain();
    assert_eq!(
        client_counters(&client.shared_for_test().lock()),
        idle(channels)
    );

    // (b) Server 包装先 Drop，Active 仍存。
    let pending = send_typed(&client, 2);
    let active = receive_typed(&server);
    drop(server);
    assert_eq!(
        service.dynamic_config().number_of_servers(),
        1,
        "Active 仍持共享状态，server 句柄尚未注销"
    );
    assert_eq!(
        pending.connection_state(),
        StrictChannelStatus::Open,
        "Active 尚活不冒 Closed"
    );
    assert!(pending.receive().expect("receive").is_none());
    assert_eq!(active.connection_state(), StrictChannelStatus::Open);
    assert!(!active.has_disconnect_hint());
    pending.set_disconnect_hint();
    assert!(
        active.has_disconnect_hint(),
        "原 hint 只匹配原请求并被 Active 看到"
    );
    assert_eq!(
        pending.connection_state(),
        StrictChannelStatus::Open,
        "hint 形态仍是原请求"
    );

    assert_eq!(reply_typed(active, 20), StrictSent { issue: None });
    assert_eq!(
        service.dynamic_config().number_of_servers(),
        0,
        "最后一个 Active 完成后共享状态退出、句柄注销"
    );
    assert_eq!(pending.connection_state(), StrictChannelStatus::Closed);
    let reply = pending
        .receive()
        .expect("receive")
        .expect("Active 发出的最后回复");
    assert_eq!(*reply, 20);
    drop(reply);
    drop(pending);
    client.maintain();
    assert_eq!(
        client_counters(&client.shared_for_test().lock()),
        idle(channels)
    );
}

// ---------------------------------------------------------------------------
// 原 route / pin 不由观察值取代：registry 空、同 index 新端点、has_data 为 false
// ---------------------------------------------------------------------------

/// 覆盖：请求随整个 server1 退出而丢失（无人 finish，channel 仍是原请求 Open + hint）。注册表变空、
/// 同 index 上出现初值 CLOSED 的新端点、原连接 `has_data == false`，都不能解除原 pin 或把 Detached
/// 当作完成回收；新代请求走另一 channel 正常往返；该残件只由最后 shared 退出本地处置。
///
/// 说明：client 侧的 `Unknown` 只在第三方清理写入 ORPHANED 时出现（需要死进程清理，不在本文件）；
/// 正常 Drop 不改 channel 状态，因此这里的残件保持 `Open`。
#[test]
fn strict_initial_closed_empty_registry_and_reused_port_keep_original_pin() {
    let config = strict_config();
    let node = create_node(&config);
    let service = typed_service(&node, 2, 1);
    exit_trace::take();
    let client = typed_strict_client(&service);
    let channels = client.number_of_channels();
    let server1 = typed_strict_server(&service);

    let pending = send_typed(&client, 31);
    let stuck_channel = pending.channel_id();
    drop(server1);
    drop(pending);
    {
        let shared = client.shared_for_test().lock();
        shared.update_connections().expect("注册表变空的 update");
        assert_eq!(
            shared.response_receiver.pin_count_at_index(0),
            None,
            "index 已空"
        );
        assert_eq!(
            shared.response_receiver.retained_len(),
            1,
            "原连接因 pin 保留"
        );
        let pin = detached_pin(&shared, stuck_channel);
        assert_eq!(shared.response_receiver.pin_count_pinned(pin), 1);
        assert_eq!(
            shared.response_receiver.strict_status_pinned(pin),
            StrictChannelStatus::Open
        );
        assert!(!shared.response_receiver.has_data_pinned(pin));
    }
    client.maintain();
    assert_eq!(
        client.number_of_detached_requests(),
        1,
        "注册表空不解除原 pin"
    );

    // 同 index 的新端点（其 channel 初值 CLOSED）。
    let server2 = typed_strict_server(&service);
    {
        let shared = client.shared_for_test().lock();
        shared.update_connections().expect("新端点占同一 index");
        assert_eq!(shared.response_receiver.pin_count_at_index(0), Some(0));
        assert_eq!(shared.response_receiver.number_of_stored_connections(), 2);
        assert_eq!(shared.response_receiver.retained_len(), 1);
    }
    client.maintain();
    {
        let shared = client.shared_for_test().lock();
        assert_eq!(
            client_counters(&shared).detached,
            1,
            "同 index 新端点的初始 CLOSED 不替原 route 完成"
        );
        let pin = detached_pin(&shared, stuck_channel);
        assert_eq!(
            shared.response_receiver.strict_status_pinned(pin),
            StrictChannelStatus::Open
        );
        assert!(
            !shared.response_receiver.has_data_pinned(pin),
            "has_data 为 false 也不解除"
        );
        assert_eq!(shared.response_receiver.pin_count_pinned(pin), 1);
    }
    assert!(
        server2.receive().expect("receive").is_none(),
        "随 server1 消失的请求不会到达新端点"
    );

    // 新代请求走另一 channel 正常往返；残件仍在。
    let pending = send_typed(&client, 32);
    assert_ne!(pending.channel_id(), stuck_channel);
    let active = receive_typed(&server2);
    assert_eq!(*active.payload(), 32);
    assert_eq!(reply_typed(active, 320), StrictSent { issue: None });
    assert_eq!(*pending.receive().expect("receive").expect("回复"), 320);
    drop(pending);
    client.maintain();
    {
        let shared = client.shared_for_test().lock();
        let counters = client_counters(&shared);
        assert_eq!(
            (counters.detached, counters.active, counters.pool),
            (1, 1, channels - 1)
        );
    }

    // 只有最后 shared 退出才本地处置该残件（不是正常回收）。
    drop(client);
    assert_eq!(exit_trace::take(), EXIT_WITH_DETACHED);
}

// ---------------------------------------------------------------------------
// exact-id：完整原 client id 的目标解析，numeric index 只是提示
// ---------------------------------------------------------------------------

/// 畸形内部 header 仍经真实原 loan/投递/接收，连接原身份不允许被 header 冒用。
#[test]
fn strict_origin_mismatch_rejects_original_sample_without_finishing_new_client() {
    let config = strict_config();
    let node = create_node(&config);
    let service = typed_service(&node, 1, 1);
    let client = typed_strict_client(&service);
    let other = typed_strict_client(&service);
    let server = typed_strict_server(&service);
    let other_pending = send_typed(&other, 90);
    let other_active = receive_typed(&server);
    let mut malformed = client.loan_uninit().expect("原 loan").write_payload(91);
    malformed.set_header_client_id_for_test(other.id());
    let pending = malformed.send().expect("真实原连接入队");
    let offset = pending.offset_for_test();
    assert!(
        matches!(server.receive(), Err(crate::strict::StrictReceiveError::OriginMismatch { origin, header_client })
        if origin == client.id().value() && header_client == other.id().value())
    );
    assert_eq!(
        other_pending.connection_state(),
        StrictChannelStatus::Open,
        "畸形 header 不关闭另一 client"
    );
    assert_eq!(
        pending.connection_state(),
        StrictChannelStatus::Open,
        "拒绝不伪造 Active finish"
    );
    client
        .shared_for_test()
        .lock()
        .request_sender
        .retrieve_returned_chunks();
    assert_eq!(
        client
            .shared_for_test()
            .lock()
            .request_sender
            .sample_reference_count_for_test(offset),
        1,
        "拒绝恰好归还接收引用"
    );
    drop(pending);
    client.maintain();
    assert_eq!(client.number_of_detached_requests(), 1);
    reply_typed(other_active, 900);
    assert_eq!(
        *other_pending.receive().expect("receive").expect("原回复"),
        900
    );
    drop(other_pending);
    other.maintain();
    assert_eq!(other.number_of_active_requests(), 0);
}

/// 同一真实连接移动到不同表下标，已有 Active 的旧 hint 由完整 client 身份重新定位。
#[test]
fn strict_exact_client_id_follows_original_connection_after_index_move() {
    let config = strict_config();
    let node = create_node(&config);
    let service = typed_service(&node, 1, 1);
    let first = typed_strict_client(&service);
    let second = typed_strict_client(&service);
    let server = typed_strict_server(&service);
    let pending1 = send_typed(&first, 81);
    let active1 = receive_typed(&server);
    let pending2 = send_typed(&second, 82);
    let active2 = receive_typed(&server);
    {
        let shared = server.shared_for_test().lock();
        let sender = &shared.response_sender;
        let a = sender
            .get_connection_id_of(first.id().value())
            .expect("第一原连接");
        let b = sender
            .get_connection_id_of(second.id().value())
            .expect("第二原连接");
        assert_ne!(a, b);
        // 持原 shared 锁，按值交换原 Connection；不复制 CAL sender，不改变身份或状态。
        unsafe {
            core::ptr::swap(sender.connections[a].get(), sender.connections[b].get());
        }
        assert_eq!(sender.get_connection_id_of(first.id().value()), Some(b));
        assert_eq!(sender.get_connection_id_of(second.id().value()), Some(a));
    }
    assert_eq!(active1.connection_state(), StrictChannelStatus::Open);
    assert_eq!(active2.connection_state(), StrictChannelStatus::Open);
    reply_typed(active1, 810);
    assert_eq!(pending1.connection_state(), StrictChannelStatus::Closed);
    assert_eq!(
        pending2.connection_state(),
        StrictChannelStatus::Open,
        "finish 没有误用旧 index"
    );
    assert!(pending2.receive().expect("第二端 receive").is_none());
    assert_eq!(
        *pending1.receive().expect("第一端 receive").expect("原回复"),
        810
    );
    reply_typed(active2, 820);
    assert_eq!(
        *pending2.receive().expect("第二端 receive").expect("原回复"),
        820
    );
    drop(pending1);
    drop(pending2);
    first.maintain();
    second.maintain();
    assert_eq!(
        first.number_of_active_requests() + second.number_of_active_requests(),
        0
    );
}

/// 真原 CAL sender 被测试 abandon 后走清理入口，Unknown 不能作为 Detached 回收依据。
#[test]
fn strict_cleanup_unknown_keeps_detached_until_local_exit() {
    use crate::service::{config_scheme::connection_config, naming_scheme::connection_name};
    use iceoryx2_bb_elementary_traits::testing::abandonable::Abandonable;
    use iceoryx2_cal::zero_copy_connection::ZeroCopyConnection;

    let config = strict_config();
    let node = create_node(&config);
    let service = typed_service(&node, 1, 1);
    let client = typed_strict_client(&service);
    let server = typed_strict_server(&service);
    let pending = send_typed(&client, 71);
    let offset = pending.offset_for_test();
    let channel = pending.channel_id();
    let active = receive_typed(&server);
    {
        let shared = server.shared_for_test().lock();
        let index = shared
            .response_sender
            .get_connection_id_of(client.id().value())
            .expect("原连接");
        let connection = unsafe { &mut *shared.response_sender.connections[index].get() }
            .take()
            .expect("原 sender 原件");
        // 测试 API 按值终止该 sender，不再存在任何活 sender 访问原连接；保持原 IOX 数据段。
        connection.sender.abandon();
        unsafe {
            <Service as crate::service::Service>::Connection::remove_sender(
                &connection_name(server.id().value(), client.id().value()),
                &connection_config::<Service>(&config),
            )
        }
        .expect("真实 CAL 清理");
    }
    assert_eq!(pending.connection_state(), StrictChannelStatus::Unknown);
    drop(pending);
    drop(active);
    for _ in 0..2 {
        client.maintain();
    }
    {
        let shared = client.shared_for_test().lock();
        shared.request_sender.retrieve_returned_chunks();
        let pin = detached_pin(&shared, channel);
        assert_eq!(
            shared.response_receiver.strict_status_pinned(pin),
            StrictChannelStatus::Unknown
        );
        assert_eq!(shared.response_receiver.pin_count_pinned(pin), 1);
        assert_eq!(shared.response_receiver.borrow_count_pinned(pin), 0);
        assert!(!shared.response_receiver.has_data_pinned(pin));
        assert_eq!(
            shared
                .request_sender
                .sample_reference_count_for_test(offset),
            1
        );
        assert_eq!(
            (
                client_counters(&shared).active,
                client_counters(&shared).detached
            ),
            (1, 1)
        );
        assert_eq!(
            client_counters(&shared).pool + 1,
            shared.strict_slots().len()
        );
    }
    exit_trace::take();
    drop(client);
    assert_eq!(exit_trace::take(), EXIT_WITH_DETACHED);
}

/// 真实 borrow 上限导致原接收错误；维护不得把错误当空队列而归还仍借出的原件。
#[test]
fn strict_detached_receive_error_preserves_original_borrow() {
    let config = strict_config();
    let node = create_node(&config);
    let service = typed_builder(&node, 1, 1)
        .max_borrowed_responses_per_pending_response(1)
        .create()
        .expect("单 borrow 服务");
    let client = typed_strict_client(&service);
    let server = typed_strict_server(&service);
    let pending = send_typed(&client, 72);
    let channel = pending.channel_id();
    reply_typed(receive_typed(&server), 720);
    let reply = pending.receive().expect("receive").expect("真实外持回复");
    assert!(matches!(
        pending.receive(),
        Err(ReceiveError::ExceedsMaxBorrows)
    ));
    drop(pending);
    client.maintain();
    {
        let shared = client.shared_for_test().lock();
        let pin = detached_pin(&shared, channel);
        assert_eq!(shared.response_receiver.borrow_count_pinned(pin), 1);
        assert_eq!(shared.response_receiver.pin_count_pinned(pin), 1);
        assert_eq!(
            (
                client_counters(&shared).active,
                client_counters(&shared).detached
            ),
            (1, 1)
        );
    }
    drop(reply);
    client.maintain();
    assert_eq!(
        client_counters(&client.shared_for_test().lock()),
        idle(client.number_of_channels())
    );
}

/// 覆盖：server 持有 client1 的 Active 时 client1 整个退出、client2 复用同一 index，并以同 channel /
/// 同序号发出自己的请求：原 Active 的状态在 update 后为 `Unknown`（之前能看到原 hint），唯一回复
/// `TargetChanged`（typed，本地 loan 与 sample 释放），不向 client2 发送 / 关闭 / hint；client2 只收到
/// 自己的回复。
#[test]
fn strict_exact_client_id_prevents_new_generation_delivery() {
    let config = strict_config();
    let node = create_node(&config);
    let service = typed_builder(&node, 1, 1)
        .max_clients(1)
        .create()
        .expect("创建单 client 服务");
    let server = typed_strict_server(&service);
    let client1 = typed_strict_client(&service);
    let client1_id = client1.id();

    let pending1 = send_typed(&client1, 11);
    let stale = receive_typed(&server);
    assert_eq!(stale.origin(), client1_id);
    assert_eq!(stale.connection_state(), StrictChannelStatus::Open);
    drop(pending1);
    assert!(stale.has_disconnect_hint(), "Pending Drop 只发原 hint");
    drop(client1);
    assert_eq!(service.dynamic_config().number_of_clients(), 0);
    // server 尚未 update：原连接仍在 index 0，原 hint 形态仍可见。
    assert_eq!(stale.connection_state(), StrictChannelStatus::Open);

    let client2 = typed_strict_client(&service);
    assert_ne!(client2.id(), client1_id);
    let pending2 = send_typed(&client2, 22);
    assert_eq!(pending2.channel_id(), stale.channel_id(), "同 channel");
    assert_eq!(pending2.request_id(), stale.request_id(), "同序号");
    assert!(server.has_requests().expect("has_requests 触发 update"));
    {
        let shared = server.shared_for_test().lock();
        assert_eq!(
            shared
                .response_sender
                .get_connection_id_of(client2.id().value()),
            Some(0),
            "client2 复用了 index 0"
        );
        assert_eq!(
            shared
                .response_sender
                .get_connection_id_of(client1_id.value()),
            None
        );
    }
    assert_eq!(stale.connection_state(), StrictChannelStatus::Unknown);
    assert!(
        !stale.has_disconnect_hint(),
        "原 client 缺失即 false，不读新代的状态"
    );

    let response = stale
        .loan_uninit()
        .expect("本地 loan 与目标无关")
        .write_payload(1111);
    let offset = response.offset_for_test();
    assert_eq!(
        server
            .shared_for_test()
            .lock()
            .response_sender
            .loan_counter
            .load(Ordering::Relaxed),
        1
    );
    assert_eq!(response.send(), Err(StrictResponseSendError::TargetChanged));
    {
        let shared = server.shared_for_test().lock();
        assert_eq!(
            shared.response_sender.loan_counter.load(Ordering::Relaxed),
            0
        );
        assert_eq!(
            shared
                .response_sender
                .sample_reference_count_for_test(offset),
            0
        );
    }
    assert_eq!(
        pending2.connection_state(),
        StrictChannelStatus::Open,
        "新代 channel 未被关闭"
    );
    assert!(!pending2.has_response(), "没有向新代投递");

    let active2 = receive_typed(&server);
    assert_eq!(active2.origin(), client2.id());
    assert_eq!(*active2.payload(), 22);
    assert_eq!(reply_typed(active2, 2222), StrictSent { issue: None });
    let reply = pending2
        .receive()
        .expect("receive")
        .expect("client2 自己的回复");
    assert_eq!(*reply, 2222);
    drop(reply);
    assert!(
        pending2.receive().expect("receive").is_none(),
        "没有混入原 client 的回复"
    );
}

/// 覆盖：client2 先于 client1 退出前建立（占 index 1），client1 退出后原 index 0 为空：原 Active 的
/// 状态 `Unknown`、唯一回复 `TargetGone`，本地 loan 释放；client2 不受影响。
#[test]
fn strict_exact_client_id_reports_target_gone_when_index_is_empty() {
    let config = strict_config();
    let node = create_node(&config);
    let service = typed_builder(&node, 1, 1)
        .max_clients(2)
        .create()
        .expect("创建双 client 服务");
    let server = typed_strict_server(&service);
    let client1 = typed_strict_client(&service);
    let client2 = typed_strict_client(&service);
    let client1_id = client1.id();

    let pending1 = send_typed(&client1, 11);
    let stale = receive_typed(&server);
    assert_eq!(stale.origin(), client1_id);
    drop(pending1);
    drop(client1);
    assert!(!server.has_requests().expect("has_requests 触发 update"));
    {
        let shared = server.shared_for_test().lock();
        assert_eq!(
            shared
                .response_sender
                .get_connection_id_of(client1_id.value()),
            None
        );
        assert_eq!(
            shared
                .response_sender
                .get_connection_id_of(client2.id().value()),
            Some(1)
        );
    }
    assert_eq!(stale.connection_state(), StrictChannelStatus::Unknown);
    assert!(!stale.has_disconnect_hint());

    let response = stale.loan_uninit().expect("本地 loan").write_payload(1);
    assert_eq!(response.send(), Err(StrictResponseSendError::TargetGone));
    assert_eq!(
        server
            .shared_for_test()
            .lock()
            .response_sender
            .loan_counter
            .load(Ordering::Relaxed),
        0
    );

    let pending2 = send_typed(&client2, 22);
    let active2 = receive_typed(&server);
    assert_eq!(active2.origin(), client2.id());
    assert_eq!(reply_typed(active2, 2222), StrictSent { issue: None });
    assert_eq!(*pending2.receive().expect("receive").expect("回复"), 2222);
}

// ---------------------------------------------------------------------------
// Pending 提前 Drop：Detached 只发原 hint，等原正常 Closed + 真排空 + borrow 0 才回收
// ---------------------------------------------------------------------------

/// 覆盖：Pending 早 Drop → 原 hint（server 可见）→ Detached；channel 仍 Open 时维护不回收；
/// server 之后的真实回复落入无人等待的队列（Closed + has_data），维护先真收并归还该回复才一次
/// 释放 offset / active / channel / pin；同一轮维护不消费 Live 请求的回复。
#[test]
fn strict_pending_early_drop_waits_for_original_close() {
    let config = strict_config();
    let node = create_node(&config);
    let service = typed_service(&node, 2, 2);
    let client = typed_strict_client(&service);
    let channels = client.number_of_channels();
    let server = typed_strict_server(&service);

    let pending1 = send_typed(&client, 1);
    let pending2 = send_typed(&client, 2);
    let channel1 = pending1.channel_id();
    let offset1 = pending1.offset_for_test();
    let active1 = receive_typed(&server);
    let active2 = receive_typed(&server);
    assert_eq!(*active1.payload(), 1);
    assert_eq!(*active2.payload(), 2);

    drop(pending1);
    assert!(active1.has_disconnect_hint());
    assert!(!active2.has_disconnect_hint());
    assert_eq!(active1.connection_state(), StrictChannelStatus::Open);
    {
        let shared = client.shared_for_test().lock();
        let counters = client_counters(&shared);
        assert_eq!(
            (counters.detached, counters.live, counters.active),
            (1, 1, 2)
        );
        let pin = detached_pin(&shared, channel1);
        assert_eq!(
            shared.response_receiver.strict_status_pinned(pin),
            StrictChannelStatus::Open
        );
    }
    client.maintain();
    assert_eq!(
        client.number_of_detached_requests(),
        1,
        "channel 仍 Open：不回收"
    );

    // server 回复两份：第一份无人等待，第二份属于 Live 的 pending2。
    assert_eq!(reply_typed(active1, 10), StrictSent { issue: None });
    assert_eq!(reply_typed(active2, 20), StrictSent { issue: None });
    {
        let shared = client.shared_for_test().lock();
        let pin = detached_pin(&shared, channel1);
        assert_eq!(
            shared.response_receiver.strict_status_pinned(pin),
            StrictChannelStatus::Closed
        );
        assert!(
            shared.response_receiver.has_data_pinned(pin),
            "无人等待的真实回复在队列里"
        );
        assert_eq!(shared.response_receiver.borrow_count_pinned(pin), 0);
    }
    client.maintain();
    {
        let shared = client.shared_for_test().lock();
        assert_eq!(
            client_counters(&shared),
            ClientCounters {
                loans: 0,
                active: 1,
                pool: channels - 1,
                free: channels - 1,
                live: 1,
                detached: 0,
            },
            "真收并归还回复后一次释放 offset / active / channel / pin"
        );
        assert!(!shared.response_receiver.has_chunks(channel1), "回复已归还");
        assert_eq!(
            shared.response_receiver.pin_count_at_index(0),
            Some(1),
            "只剩 Live 的 pin"
        );
        shared.request_sender.retrieve_returned_chunks();
        assert_eq!(
            shared
                .request_sender
                .sample_reference_count_for_test(offset1),
            0
        );
    }

    // Live 请求的回复不被维护消费。
    assert!(pending2.has_response());
    let reply = pending2
        .receive()
        .expect("receive")
        .expect("Live 请求的回复仍在");
    assert_eq!(*reply, 20);
    drop(reply);
    drop(pending2);
    client.maintain();
    assert_eq!(
        client_counters(&client.shared_for_test().lock()),
        idle(channels)
    );
}

/// 覆盖：N = 1 时 Detached 的 channel 只有在真实回复被维护收走并归还后才回到池中；下一次 loan 拿到
/// 同一 channel，新序号的请求正常往返，被排空的旧回复不会混入。
#[test]
fn strict_detached_drains_real_reply_before_reuse() {
    let config = strict_config();
    let node = create_node(&config);
    let service = typed_service(&node, 1, 1);
    let client = typed_strict_client_with_channels(&service, 1);
    let server = typed_strict_server(&service);

    let pending = send_typed(&client, 1);
    assert_eq!(pending.channel_id(), CH0);
    let active = receive_typed(&server);
    drop(pending);
    assert!(active.has_disconnect_hint());
    assert_eq!(reply_typed(active, 10), StrictSent { issue: None });
    {
        let shared = client.shared_for_test().lock();
        assert_eq!(client_counters(&shared).detached, 1);
        assert_eq!(client_counters(&shared).pool, 0);
        assert!(
            shared.response_receiver.has_chunks(CH0),
            "无人等待的回复尚未排空"
        );
    }

    // loan 内核先维护：真收并归还旧回复、回收槽，再分配同一 channel。
    let request = client.loan_uninit().expect("排空后可再次 loan");
    assert_eq!(request.header().channel_id, CH0, "复用同一 channel");
    assert_eq!(request.header().request_id.value(), 2);
    {
        let shared = client.shared_for_test().lock();
        assert_eq!(
            client_counters(&shared),
            ClientCounters {
                loans: 1,
                active: 0,
                pool: 0,
                free: 1,
                live: 0,
                detached: 0,
            }
        );
        assert!(
            !shared.response_receiver.has_chunks(CH0),
            "旧回复已被维护归还"
        );
        assert_eq!(shared.response_receiver.pin_count_at_index(0), Some(0));
    }
    let pending = request.write_payload(2).send().expect("新序号发送");
    let active = receive_typed(&server);
    assert_eq!(active.request_id().value(), 2);
    assert_eq!(*active.payload(), 2);
    assert_eq!(reply_typed(active, 20), StrictSent { issue: None });
    let reply = pending.receive().expect("receive").expect("新请求的回复");
    assert_eq!(*reply, 20, "不是被排空的旧回复");
    drop(reply);
    assert!(pending.receive().expect("receive").is_none());
    drop(pending);
    client.maintain();
    assert_eq!(client_counters(&client.shared_for_test().lock()), idle(1));
}

// ---------------------------------------------------------------------------
// Response 两种释放顺序与最后 shared 退出
// ---------------------------------------------------------------------------

/// 覆盖：先 Drop Pending 与 Client、外持 Response：Response 仍持 shared（无退出记录、载荷可读、
/// borrow 保持），Drop Response 归还 borrow 后才触发最后 shared 退出并处置该 Detached。
#[test]
fn strict_response_arc_delays_last_shared_drop() {
    let config = strict_config();
    let node = create_node(&config);
    let service = typed_service(&node, 1, 1);
    exit_trace::take();
    let client = typed_strict_client(&service);
    let server = typed_strict_server(&service);

    let pending = send_typed(&client, 1);
    assert_eq!(
        reply_typed(receive_typed(&server), 10),
        StrictSent { issue: None }
    );
    let reply = pending.receive().expect("receive").expect("回复");
    assert_eq!(
        client
            .shared_for_test()
            .lock()
            .response_receiver
            .borrow_count_pinned(pending.pin_for_test()),
        1
    );
    drop(pending);
    drop(client);
    assert!(
        exit_trace::take().is_empty(),
        "Response 仍持 shared：最后 shared 尚未退出"
    );
    assert_eq!(*reply, 10, "外持 Response 的载荷仍可读");
    assert_eq!(
        service.dynamic_config().number_of_clients(),
        1,
        "句柄仍登记"
    );

    drop(reply);
    assert_eq!(exit_trace::take(), EXIT_WITH_DETACHED);
    assert_eq!(service.dynamic_config().number_of_clients(), 0);
}

/// 覆盖：先 Drop Response（borrow 1 → 0 只一次）再 Drop Pending：Detached 在 Closed + 排空 + borrow 0
/// 下由正常维护一次回收（原 pin 归零）；随后最后 shared 退出没有任何 Detached 处置。
#[test]
fn strict_response_drop_before_pending_preserves_single_disposal() {
    let config = strict_config();
    let node = create_node(&config);
    let service = typed_service(&node, 1, 1);
    exit_trace::take();
    let client = typed_strict_client(&service);
    let channels = client.number_of_channels();
    let server = typed_strict_server(&service);

    let pending = send_typed(&client, 1);
    assert_eq!(
        reply_typed(receive_typed(&server), 10),
        StrictSent { issue: None }
    );
    let reply = pending.receive().expect("receive").expect("回复");
    {
        let shared = client.shared_for_test().lock();
        assert_eq!(
            shared
                .response_receiver
                .borrow_count_pinned(pending.pin_for_test()),
            1
        );
    }
    drop(reply);
    {
        let shared = client.shared_for_test().lock();
        assert_eq!(
            shared
                .response_receiver
                .borrow_count_pinned(pending.pin_for_test()),
            0,
            "borrow 只归还一次"
        );
        assert_eq!(
            shared
                .response_receiver
                .pin_count_pinned(pending.pin_for_test()),
            1
        );
    }
    drop(pending);
    client.maintain();
    {
        let shared = client.shared_for_test().lock();
        assert_eq!(client_counters(&shared), idle(channels));
        assert_eq!(
            shared.response_receiver.pin_count_at_index(0),
            Some(0),
            "原 pin 消费一次"
        );
    }
    drop(client);
    assert_eq!(
        exit_trace::take(),
        EXIT_WITHOUT_DETACHED,
        "正常回收后退出没有第二次处置"
    );
}

/// 覆盖：send 后 Pending Drop（server 未回复，channel 仍 Open）→ Detached；最后 shared 退出的真实
/// 顺序恰为：按固定槽处置 Detached 本地原件（含原 pin）→ 注销 client 句柄 → 字段析构；退出后注册表
/// 不再有该 client，server 侧仍活的 Active 之后按精确 id finish 不受影响。
#[test]
fn strict_last_shared_drop_disposes_detached_before_registry_and_tag() {
    let config = strict_config();
    let node = create_node(&config);
    let service = typed_service(&node, 1, 1);
    exit_trace::take();
    let client = typed_strict_client(&service);
    let server = typed_strict_server(&service);

    let pending = send_typed(&client, 1);
    let active = receive_typed(&server);
    drop(pending);
    client.maintain();
    assert_eq!(
        client.number_of_detached_requests(),
        1,
        "channel 仍 Open：维护不回收"
    );
    assert_eq!(service.dynamic_config().number_of_clients(), 1);

    exit_trace::start_details();
    drop(client);
    assert_eq!(
        exit_trace::take_details(),
        [
            "shared_drop:dispose_detached",
            "request:sample_released",
            "request:active_decremented",
            "request:channel_returned",
            "request:pin_consumed",
            "shared_drop:detached_released",
            "shared_drop:client_handle_released",
            "shared_drop:body_end_fields_follow",
            "fields:request_sender_dropped",
            "fields:response_receiver_dropped",
            "fields:port_tag_dropped",
        ],
        "操作后观测与字段后置析构标记证明真实退出顺序"
    );
    assert_eq!(exit_trace::take(), EXIT_WITH_DETACHED);
    assert_eq!(
        service.dynamic_config().number_of_clients(),
        0,
        "句柄已注销"
    );

    // server 侧仍活的 Active 不受影响，Drop 时按精确原 id finish。
    assert!(active.has_disconnect_hint());
    drop(active);
}

/// 覆盖：与端点退出相对的正常回收——Closed + 排空 + borrow 0 后维护一次归还 offset / active /
/// channel / pin，channel 回池后可再次 loan 到同一 channel id；退出时没有 Detached 处置。
#[test]
fn strict_normal_detached_reclaim_reuses_channel() {
    let config = strict_config();
    let node = create_node(&config);
    let service = typed_service(&node, 1, 1);
    exit_trace::take();
    let client = typed_strict_client_with_channels(&service, 1);
    let server = typed_strict_server(&service);

    let pending = send_typed(&client, 1);
    let offset = pending.offset_for_test();
    assert_eq!(
        reply_typed(receive_typed(&server), 10),
        StrictSent { issue: None }
    );
    let reply = pending.receive().expect("receive").expect("回复");
    assert_eq!(*reply, 10);
    drop(reply);
    drop(pending);
    {
        let shared = client.shared_for_test().lock();
        let counters = client_counters(&shared);
        assert_eq!(
            (counters.detached, counters.active, counters.pool),
            (1, 1, 0)
        );
        assert_eq!(shared.response_receiver.pin_count_at_index(0), Some(1));
    }
    client.maintain();
    {
        let shared = client.shared_for_test().lock();
        assert_eq!(client_counters(&shared), idle(1));
        assert_eq!(shared.response_receiver.pin_count_at_index(0), Some(0));
        assert!(!shared.response_receiver.has_chunks(CH0));
        shared.request_sender.retrieve_returned_chunks();
        assert_eq!(
            shared
                .request_sender
                .sample_reference_count_for_test(offset),
            0
        );
    }

    let request = client.loan_uninit().expect("回收后再次 loan");
    assert_eq!(
        request.header().channel_id,
        CH0,
        "同一 channel id 回池后复用"
    );
    assert_eq!(request.header().request_id.value(), 2);
    let pending = request.write_payload(2).send().expect("send");
    assert_eq!(
        reply_typed(receive_typed(&server), 20),
        StrictSent { issue: None }
    );
    assert_eq!(*pending.receive().expect("receive").expect("回复"), 20);
    drop(pending);
    client.maintain();
    assert_eq!(client_counters(&client.shared_for_test().lock()), idle(1));

    drop(client);
    assert_eq!(
        exit_trace::take(),
        EXIT_WITHOUT_DETACHED,
        "正常回收不计作退出处置"
    );
}

// ---------------------------------------------------------------------------
// 按值唯一回复：成功 / 未初始化 Drop / 已初始化未发送 Drop / loan 失败
// ---------------------------------------------------------------------------

/// 覆盖：StrictActive 按值进入唯一回复 loan；成功发送、未初始化 Drop、已初始化未发送 Drop 三条路径
/// 都先释放本地 loan 再 finish 原 Active（client 观察 Closed、回复有无正确）；loan 失败按原样返回
/// Active（仍 Open、无 loan 残留）并可继续正常回复；client 侧切片长度拒绝不动池位置。
/// “无第二次回复”由 `send(self)` 按值消费在类型层保证，运行期无法构造。
#[test]
fn strict_unique_response_finishes_after_local_loan() {
    let config = strict_config();
    let node = create_node(&config);
    let service = slice_service(&node, 2, 2);
    let client = slice_strict_client(&service, 4);
    let channels = client.number_of_channels();
    let server = slice_strict_server(&service, 4);
    let server_loans = || {
        server
            .shared_for_test()
            .lock()
            .response_sender
            .loan_counter
            .load(Ordering::Relaxed)
    };

    // (1) 成功发送。
    let pending = send_slice(&client, &[1]);
    let active = receive_slice(&server);
    let response = active
        .loan_slice_uninit(2)
        .expect("loan")
        .write_from_slice(&[10, 11]);
    assert_eq!(server_loans(), 1);
    assert_eq!(response.request().request_id(), pending.request_id());
    assert_eq!(response.request().origin(), client.id());
    assert_eq!(
        pending.connection_state(),
        StrictChannelStatus::Open,
        "loan 期间原 Active 未 finish"
    );
    assert_eq!(
        response.send().expect("唯一回复"),
        StrictSent { issue: None }
    );
    assert_eq!(server_loans(), 0);
    assert_eq!(pending.connection_state(), StrictChannelStatus::Closed);
    let reply = pending.receive().expect("receive").expect("回复");
    assert_eq!(reply.payload(), &[10, 11]);
    drop(reply);
    drop(pending);

    // (2) 未初始化 Drop。
    let pending = send_slice(&client, &[2]);
    let active = receive_slice(&server);
    let uninit = active.loan_slice_uninit(1).expect("loan");
    assert_eq!(server_loans(), 1);
    assert_eq!(pending.connection_state(), StrictChannelStatus::Open);
    drop(uninit);
    assert_eq!(server_loans(), 0, "本地 loan 先释放");
    assert_eq!(
        pending.connection_state(),
        StrictChannelStatus::Closed,
        "随后 finish 原 Active"
    );
    assert!(pending.receive().expect("receive").is_none());
    drop(pending);

    // (3) 已初始化、未发送 Drop。
    let pending = send_slice(&client, &[3]);
    let active = receive_slice(&server);
    let response = active
        .loan_slice_uninit(1)
        .expect("loan")
        .write_from_slice(&[30]);
    assert_eq!(pending.connection_state(), StrictChannelStatus::Open);
    drop(response);
    assert_eq!(server_loans(), 0);
    assert_eq!(pending.connection_state(), StrictChannelStatus::Closed);
    assert!(pending.receive().expect("receive").is_none());
    drop(pending);

    // (4) loan 失败：Active 原样返回，仍 Open，无 loan 残留；随后正常回复。
    let pending = send_slice(&client, &[4]);
    let active = receive_slice(&server);
    let refused = active
        .loan_slice_uninit(5)
        .expect_err("超过 initial_max_slice_len 必须拒绝");
    assert_eq!(refused.error, LoanError::ExceedsMaxLoanSize);
    assert_eq!(server_loans(), 0);
    let active = refused.active;
    assert_eq!(active.connection_state(), StrictChannelStatus::Open);
    assert_eq!(pending.connection_state(), StrictChannelStatus::Open);
    assert_eq!(*active.payload().first().expect("载荷"), 4);
    let sent = active
        .loan_slice_uninit(1)
        .expect("loan")
        .write_from_slice(&[40])
        .send()
        .expect("拒绝后仍可正常回复");
    assert_eq!(sent, StrictSent { issue: None });
    let reply = pending.receive().expect("receive").expect("回复");
    assert_eq!(reply.payload(), &[40]);
    drop(reply);
    drop(pending);

    client.maintain();
    assert_eq!(
        client_counters(&client.shared_for_test().lock()),
        idle(channels)
    );
    assert_eq!(server_loans(), 0);
}
