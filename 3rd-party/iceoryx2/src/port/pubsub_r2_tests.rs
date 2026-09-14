// RR R2（robot-system 本地补丁，2026-09-14）：通用 pub/sub 的两条公用行为门。
//
// 目的：R1 把 CAL 的 `remove_port` 改为真实 CAS→ORPHANED、`blocking_send` 识别两个终止值，
// 这是 pub/sub 与 RR 共用的公用路径。本文件在 iceoryx2 库内以真实 SHM（`ipc_threadsafe::Service`，
// 与 `ipc::Service` 使用同一套 posix 概念，只是 Arc 策略可跨线程）证明：
//   1. 活 subscriber 满队列时 RetryUntilDelivered 仍真实等待，真实消费后继续投递；
//      DiscardData / overflow / 普通 borrow 的原分类与 recipients 计数不变；
//   2. 真子进程 subscriber 被 SIGKILL 并 reap 后，由同批 patched `Node::try_cleanup_dead_nodes`
//      走真实清理链（service 死节点移除 → `remove_receiver_port_from_all_connections` →
//      CAL `remove_port` 的同源 `mark_orphaned`），满队列上等待的 Retry 发送方有界退出且该次
//      recipients=0。不用 drop 活对象或直接写 raw 状态冒充死亡清理。
//
// 运行（仓库根）：
//   cargo test --locked --manifest-path 3rd-party/Cargo.toml --target-dir target/rr-vendor-tests \
//       -p iceoryx2 --lib
//
// 边界：不覆盖 RR strict 协议，不是 Orin / ARM 证明；所有等待有界（10 s），失败 panic 文本说明
// 卡在哪一步；异常路径经 RAII 让阻塞发送方与子进程都能退出。
//
// 许可：与所在 crate 相同（MIT OR Apache-2.0）。

use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use core::time::Duration;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Instant;

use iceoryx2_bb_container::semantic_string::SemanticString;
use iceoryx2_bb_elementary::CallbackProgression;
use iceoryx2_bb_posix::config::TEST_DIRECTORY;
use iceoryx2_bb_posix::testing::create_test_directory;
use iceoryx2_bb_system_types::file_name::FileName;
use iceoryx2_cal::arc_sync_policy::ArcSyncPolicy;

use crate::config::Config;
use crate::node::{Node, NodeBuilder, NodeState};
use crate::port::publisher::Publisher;
use crate::port::update_connections::UpdateConnections;
use crate::port::{BackpressureAction, BackpressureInfo, ReceiveError};
use crate::prelude::{BackpressureStrategy, PortFactory, ServiceName};
use crate::service::ipc_threadsafe;
use crate::service::port_factory::publish_subscribe::PortFactory as PubSubFactory;
use crate::testing::{generate_isolated_config, generate_node_name, generate_service_name};

/// 真实 SHM 的服务类型；线程安全 Arc 策略让 publisher 可在 scoped 线程里阻塞发送。
type Service = ipc_threadsafe::Service;

const BOUND: Duration = Duration::from_secs(10);
const CHILD_ROLE_ENV: &str = "RR_R2_CHILD_ROLE";
const CHILD_PREFIX_ENV: &str = "RR_R2_PREFIX";
const CHILD_SERVICE_ENV: &str = "RR_R2_SERVICE";
const CHILD_TEST_PATH: &str = "port::pubsub_r2_tests::child_subscriber_role";

fn wait_until<F: Fn() -> bool>(what: &str, condition: F) {
    let deadline = Instant::now() + BOUND;
    while !condition() {
        assert!(Instant::now() < deadline, "等待超时（{BOUND:?}）: {what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// 异常路径也会置位的 RAII 收尾：断言失败 / 超时 panic 时同样把 `abort` 置位，
/// 让轮询它的背压回调改为 DiscardDataAndFail，阻塞发送方有界退出，scope 的自动 join 不会永挂。
struct SetOnDrop<'a>(&'a AtomicBool);

impl Drop for SetOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// 子进程守卫：任何退出路径都先 kill、再 wait（真实 reap）、最后回收 stdout reader 线程
/// （子进程退出后管道 EOF，reader 自然结束）。失败 / 超时路径同序，绝不先 join reader 死等。
struct KillOnDrop {
    child: Option<Child>,
    reader: Option<JoinHandle<()>>,
}

impl KillOnDrop {
    fn new(child: Child) -> Self {
        Self {
            child: Some(child),
            reader: None,
        }
    }

    fn child_mut(&mut self) -> &mut Child {
        self.child.as_mut().expect("子进程句柄")
    }

    /// kill → wait（reap）→ join reader；返回真实退出状态。
    fn kill_and_wait(&mut self) -> ExitStatus {
        let mut child = self.child.take().expect("子进程只能 reap 一次");
        let _ = child.kill();
        let status = child.wait().expect("reap 子进程");
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        status
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

/// 背压回调（必须 `'static`，因此持 Arc）：正常 Retry；`abort` 置位后改为 DiscardDataAndFail。
/// 每轮都会被调用，`retries` 用作真实握手：>0 即发送方确实在满队列上等待。
fn retry_until_abort(
    retries: Arc<AtomicUsize>,
    abort: Arc<AtomicBool>,
) -> impl Fn(&BackpressureInfo) -> BackpressureAction + Send + 'static {
    move |_info| {
        retries.fetch_add(1, Ordering::Relaxed);
        if abort.load(Ordering::Acquire) {
            BackpressureAction::DiscardDataAndFail
        } else {
            BackpressureAction::Retry
        }
    }
}

/// 两端（父/子进程、各测试）都用同一组服务参数：buffer 1、borrow 1、≤2 subscriber、1 publisher。
fn open_service(
    node: &Node<Service>,
    name: &ServiceName,
    overflow: bool,
) -> PubSubFactory<Service, u64, ()> {
    node.service_builder(name)
        .publish_subscribe::<u64>()
        .subscriber_max_buffer_size(1)
        .subscriber_max_borrowed_samples(1)
        .enable_safe_overflow(overflow)
        .max_subscribers(2)
        .max_publishers(1)
        .open_or_create()
        .expect("打开 / 创建 pub/sub 服务")
}

fn retry_publisher(
    service: &PubSubFactory<Service, u64, ()>,
    retries: &Arc<AtomicUsize>,
    abort: &Arc<AtomicBool>,
) -> Publisher<Service, u64, ()> {
    service
        .publisher_builder()
        .backpressure_strategy(BackpressureStrategy::RetryUntilDelivered)
        .set_backpressure_handler(retry_until_abort(Arc::clone(retries), Arc::clone(abort)))
        .create()
        .expect("创建 Retry publisher")
}

// ---------------------------------------------------------------------------
// 门 1：活 subscriber 背压与原分类
// ---------------------------------------------------------------------------

#[test]
fn pubsub_send_receive_release_and_live_backpressure_unchanged() {
    let config = generate_isolated_config();
    let node = NodeBuilder::new()
        .config(&config)
        .create::<Service>()
        .expect("创建 node");
    let service = open_service(&node, &generate_service_name(), false);
    let subscriber = service
        .subscriber_builder()
        .buffer_size(1)
        .create()
        .expect("创建 subscriber");

    let retries = Arc::new(AtomicUsize::new(0));
    let abort = Arc::new(AtomicBool::new(false));
    let publisher = retry_publisher(&service, &retries, &abort);
    // subscriber 早于 publisher 创建：其接收侧连接在下一次 update 才建立。真实握手要求 CAL
    // 连接两端都已登记（`is_connected`），否则满队列的 Retry 发送方按原语义直接以
    // NoConnectedReceiverAndBufferIsFull 退出而不进入等待 / 回调。
    subscriber
        .update_connections()
        .expect("subscriber 建立到 publisher 的接收侧连接");

    // 空位：直接投递给 1 个 subscriber。
    assert_eq!(publisher.send_copy(1), Ok(1));

    let finished = AtomicBool::new(false);
    let result = std::thread::scope(|scope| {
        let abort_on_drop = SetOnDrop(&abort);
        let worker = scope.spawn(|| {
            let result = publisher.send_copy(2);
            finished.store(true, Ordering::Release);
            result
        });

        // 真实握手：背压回调至少被调用一次 = 发送方确实在等待活 subscriber 消费。
        wait_until("背压回调被调用", || {
            retries.load(Ordering::Relaxed) > 0
        });
        assert!(
            !finished.load(Ordering::Acquire),
            "活 subscriber 未消费时发送方不得提前返回"
        );

        // 真实消费并释放（Sample Drop 即真实归还）后，发送方继续投递。
        let first = subscriber.receive().expect("接收").expect("应收到 1");
        assert_eq!(*first, 1);
        drop(first);
        wait_until("发送方在消费后返回", || {
            finished.load(Ordering::Acquire)
        });
        let result = worker.join().expect("发送线程");
        drop(abort_on_drop);
        result
    });
    assert_eq!(result, Ok(1));

    let second = subscriber.receive().expect("接收").expect("应收到 2");
    assert_eq!(*second, 2);
    drop(second);
    assert!(subscriber.receive().expect("接收").is_none());
}

#[test]
fn pubsub_discard_and_overflow_keep_original_recipient_counts() {
    let config = generate_isolated_config();
    let node = NodeBuilder::new()
        .config(&config)
        .create::<Service>()
        .expect("创建 node");

    // DiscardData（无回调）：满队列不等待，该 subscriber 不计入 recipients。
    {
        let service = open_service(&node, &generate_service_name(), false);
        let subscriber = service
            .subscriber_builder()
            .buffer_size(1)
            .create()
            .expect("创建 subscriber");
        let publisher = service
            .publisher_builder()
            .backpressure_strategy(BackpressureStrategy::DiscardData)
            .create()
            .expect("创建 DiscardData publisher");

        assert_eq!(publisher.send_copy(1), Ok(1));
        assert_eq!(
            publisher.send_copy(2),
            Ok(0),
            "满队列上 DiscardData 不计入 recipients"
        );
        let sample = subscriber.receive().expect("接收").expect("应收到 1");
        assert_eq!(*sample, 1);
        drop(sample);
        assert!(subscriber.receive().expect("接收").is_none());

        // 普通 borrow：持一份样本时再收超出 borrow 上限；释放后可再收。
        assert_eq!(publisher.send_copy(3), Ok(1));
        let held = subscriber.receive().expect("接收").expect("应收到 3");
        assert_eq!(*held, 3);
        assert_eq!(publisher.send_copy(4), Ok(1));
        assert!(matches!(
            subscriber.receive(),
            Err(ReceiveError::ExceedsMaxBorrows)
        ));
        drop(held);
        let next = subscriber.receive().expect("接收").expect("应收到 4");
        assert_eq!(*next, 4);
    }

    // overflow：两次发送都计 1 个 recipient，subscriber 只拿到最新一份。
    {
        let service = open_service(&node, &generate_service_name(), true);
        let subscriber = service
            .subscriber_builder()
            .buffer_size(1)
            .create()
            .expect("创建 subscriber");
        let publisher = service
            .publisher_builder()
            .backpressure_strategy(BackpressureStrategy::DiscardData)
            .create()
            .expect("创建 publisher");

        assert_eq!(publisher.send_copy(1), Ok(1));
        assert_eq!(publisher.send_copy(2), Ok(1));
        let latest = subscriber.receive().expect("接收").expect("应收到最新值");
        assert_eq!(*latest, 2);
        drop(latest);
        assert!(subscriber.receive().expect("接收").is_none());
    }
}

// ---------------------------------------------------------------------------
// 门 2：真子进程 subscriber 死亡 → 同批 patched Node 清理 → 满队列 Retry 发送方有界退出
// ---------------------------------------------------------------------------

/// 子进程角色入口（父测试专用，标记 `#[ignore]`）：默认套件不执行它，不会把空 return 计成
/// 一条通过；父测试用 `--exact --ignored` 重新执行本二进制并设置环境变量时，它在同一隔离
/// config / 同名服务上创建 Node + subscriber（buffer 1，从不消费），打印
/// `READY <node_id> <subscriber_id> <service_unique_id> <service_name>` 后睡眠直到被 SIGKILL。
/// 无环境变量时（例如有人手动 `--ignored` 跑全部）直接 panic，避免误判。
#[test]
#[ignore = "父测试 pubsub_dead_subscriber_cleanup_releases_full_retry_sender 专用的子进程角色入口"]
fn child_subscriber_role() {
    assert_eq!(
        std::env::var(CHILD_ROLE_ENV).as_deref(),
        Ok("subscriber"),
        "child_subscriber_role 只能由父测试通过环境变量启动"
    );
    let prefix = std::env::var(CHILD_PREFIX_ENV).expect("子进程需要 RR_R2_PREFIX");
    let service_name = std::env::var(CHILD_SERVICE_ENV).expect("子进程需要 RR_R2_SERVICE");

    // 重建与父进程完全相同的隔离 config（同 root path + 同 prefix）。
    create_test_directory();
    let mut config = Config::default();
    config.global.set_root_path(&TEST_DIRECTORY);
    config.global.prefix = FileName::new(prefix.as_bytes()).expect("合法 prefix");

    let node = NodeBuilder::new()
        .name(&generate_node_name())
        .config(&config)
        .create::<Service>()
        .expect("子进程创建 node");
    let service = open_service(
        &node,
        &ServiceName::new(&service_name).expect("合法服务名"),
        false,
    );
    let subscriber = service
        .subscriber_builder()
        .buffer_size(1)
        .create()
        .expect("子进程创建 subscriber");

    // libtest pretty 串行输出先打印无换行的 "test <name> ... "：协议行显式独立起行，父端再按子串定位。
    let mut stdout = std::io::stdout();
    writeln!(
        stdout,
        "\nREADY {} {} {} {}",
        node.id().value(),
        subscriber.id().value(),
        service.unique_service_id().value(),
        service.name().as_str()
    )
    .expect("写 READY");
    stdout.flush().expect("flush READY");

    // 从不消费、从不退出：只周期性 update 连接（父进程的 publisher 在 READY 之后才创建，
    // 接收侧连接须由本端登记才算 CAL `is_connected`），等待父进程 SIGKILL。
    loop {
        let _ = subscriber.update_connections();
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// 子进程 READY 行携带的原身份。
struct ChildReady {
    node_id: u128,
    subscriber_id: u128,
    service_id: u128,
    service_name: String,
}

/// 在有界时间内从子进程 stdout 读到 READY 行并解析原身份。reader 线程句柄交给守卫，
/// 在 kill / reap 之后（管道 EOF）才回收。
fn wait_ready_line(guard: &mut KillOnDrop) -> ChildReady {
    let stdout = guard
        .child_mut()
        .stdout
        .take()
        .expect("子进程 stdout 已 pipe");
    let (tx, rx) = mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            match line {
                Ok(line) => {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    guard.reader = Some(reader);

    let deadline = Instant::now() + BOUND;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or(Duration::ZERO);
        let line = rx
            .recv_timeout(remaining)
            .unwrap_or_else(|e| panic!("等待子进程 READY 超时或管道关闭: {e:?}"));
        // libtest 以 --nocapture 运行时先打印 "test <name> ... " 且不换行，READY 紧接其后同一行，
        // 因此按子串定位而不是行首匹配。
        if let Some(pos) = line.find("READY ") {
            let rest = &line[pos + "READY ".len()..];
            let mut parts = rest.split_whitespace();
            let mut next_u128 = |what: &str| -> u128 {
                parts
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or_else(|| panic!("READY 行缺 {what}: {line:?}"))
            };
            let node_id = next_u128("node id");
            let subscriber_id = next_u128("subscriber id");
            let service_id = next_u128("service id");
            let service_name = parts
                .next()
                .unwrap_or_else(|| panic!("READY 行缺 service name: {line:?}"))
                .to_owned();
            return ChildReady {
                node_id,
                subscriber_id,
                service_id,
                service_name,
            };
        }
    }
}

/// 查询某 node id 在 `Node::list` 中的状态（None = 未列出）。
fn node_liveness(config: &Config, node_id: u128) -> Option<&'static str> {
    let mut found = None;
    Node::<Service>::list(config, |state| {
        if state.node_id().value() == node_id {
            found = Some(match state {
                NodeState::Alive(_) => "alive",
                NodeState::Dead(_) => "dead",
                NodeState::Inaccessible(_) => "inaccessible",
                NodeState::Undefined(_) => "undefined",
            });
        }
        CallbackProgression::Continue
    })
    .expect("Node::list");
    found
}

/// 服务动态配置的 subscriber 列表里是否登记了 (subscriber_id, node_id) 这一对原身份。
fn subscriber_registered(
    service: &PubSubFactory<Service, u64, ()>,
    subscriber_id: u128,
    node_id: u128,
) -> bool {
    let mut found = false;
    service.dynamic_config().list_subscribers(|details| {
        if details.subscriber_id.value() == subscriber_id && details.node_id.value() == node_id {
            found = true;
            CallbackProgression::Stop
        } else {
            CallbackProgression::Continue
        }
    });
    found
}

#[test]
fn pubsub_dead_subscriber_cleanup_releases_full_retry_sender() {
    let config = generate_isolated_config();
    let service_name = generate_service_name();
    let node = NodeBuilder::new()
        .config(&config)
        .create::<Service>()
        .expect("创建 node");
    // 父进程先创建服务，子进程以相同参数 open。
    let service = open_service(&node, &service_name, false);

    let prefix = core::str::from_utf8(config.global.prefix.as_bytes())
        .expect("prefix 为 UTF-8")
        .to_owned();
    let exe = std::env::current_exe().expect("当前测试二进制路径");
    // 子角色入口是 #[ignore]，必须显式 --ignored 才会执行。
    let child = Command::new(&exe)
        .args([
            CHILD_TEST_PATH,
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ROLE_ENV, "subscriber")
        .env(CHILD_PREFIX_ENV, &prefix)
        .env(CHILD_SERVICE_ENV, service_name.as_str())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("启动子进程 subscriber");
    let mut child = KillOnDrop::new(child);

    let ready = wait_ready_line(&mut child);
    let child_node_id = ready.node_id;
    // 原身份核对：同一服务（unique id + name）、该 node 为 Alive、subscriber 列表里确有该
    // subscriber id 且属于该 node；不只从全局计数猜原件。
    assert_eq!(
        ready.service_id,
        service.unique_service_id().value(),
        "子进程打开的不是父进程创建的服务"
    );
    assert_eq!(ready.service_name, service_name.as_str(), "服务名不一致");
    assert_eq!(
        node_liveness(&config, child_node_id),
        Some("alive"),
        "子进程 node 在其 READY 后应为 Alive"
    );
    assert!(
        subscriber_registered(&service, ready.subscriber_id, child_node_id),
        "服务 subscriber 列表里应有子进程的 subscriber {} / node {}",
        ready.subscriber_id,
        child_node_id
    );
    assert_eq!(
        service.dynamic_config().number_of_subscribers(),
        1,
        "父进程应只看到子进程的 subscriber"
    );

    let retries = Arc::new(AtomicUsize::new(0));
    let abort = Arc::new(AtomicBool::new(false));
    let publisher = retry_publisher(&service, &retries, &abort);

    // 子进程从不消费：第一份填满其 buffer（1），recipients=1。
    assert_eq!(publisher.send_copy(1), Ok(1));
    // 真实握手：子进程的接收侧登记与父进程的发送侧登记存在竞态（子端周期性 update）。只有原
    // CAL 连接 `is_connected()` 为真，满队列的 Retry 发送方才会进入等待 / 回调；否则按原语义以
    // NoConnectedReceiverAndBufferIsFull 提前返回。这里有界等匹配 ready.subscriber_id 的原连接。
    wait_until(
        "父 publisher 到子 subscriber 的原 CAL 连接两端登记",
        || {
            publisher
                .publisher_shared_state
                .lock()
                .sender
                .connection_is_connected_for_test(ready.subscriber_id)
                == Some(true)
        },
    );

    let finished = AtomicBool::new(false);
    let mut cleanup_attempts = 0usize;
    let mut cleanups_total = 0u64;
    let mut child_status = None;
    let result = std::thread::scope(|scope| {
        let abort_on_drop = SetOnDrop(&abort);
        let worker = scope.spawn(|| {
            let result = publisher.send_copy(2);
            finished.store(true, Ordering::Release);
            result
        });

        // 真实握手：满 Retry 发送方已进入等待。
        wait_until("背压回调被调用", || {
            retries.load(Ordering::Relaxed) > 0
        });
        assert!(
            !finished.load(Ordering::Acquire),
            "子进程 subscriber 未消费时发送方不得提前返回"
        );

        // 真实死亡：SIGKILL 并 reap。
        let status = child.kill_and_wait();
        assert_eq!(
            status.signal(),
            Some(9),
            "子进程必须是被 SIGKILL 杀死并 reap，而不是自行退出 / panic: {status:?}"
        );
        child_status = Some(status);

        // 同批 patched Node 的真实清理链；文件锁释放可能略有延迟，有界重试。
        let deadline = Instant::now() + BOUND;
        loop {
            cleanup_attempts += 1;
            let state = Node::<Service>::try_cleanup_dead_nodes(&config);
            cleanups_total += state.cleanups;
            assert_eq!(
                state.failed_cleanups, 0,
                "死节点清理不应失败（第 {cleanup_attempts} 次）"
            );
            if cleanups_total >= 1 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "{cleanup_attempts} 次 try_cleanup_dead_nodes 后仍未清理到死节点"
            );
            std::thread::sleep(Duration::from_millis(100));
        }

        // 清理把 channel 真实 CAS 为 ORPHANED 并移除端点位：阻塞发送方必须有界退出。
        wait_until("清理后发送方返回", || {
            finished.load(Ordering::Acquire)
        });
        let result = worker.join().expect("发送线程");
        drop(abort_on_drop);
        result
    });
    assert_eq!(
        result,
        Ok(0),
        "死 subscriber 被清理后该次投递 recipients 必须为 0，且不是错误"
    );

    assert!(
        !subscriber_registered(&service, ready.subscriber_id, child_node_id),
        "清理后子进程的 subscriber {} 应从服务注销",
        ready.subscriber_id
    );
    assert_eq!(
        service.dynamic_config().number_of_subscribers(),
        0,
        "清理后服务不应再有 subscriber"
    );
    let liveness = node_liveness(&config, child_node_id);
    assert_ne!(
        liveness,
        Some("alive"),
        "被清理的子 node 不应再为 Alive: {liveness:?}"
    );
    assert_eq!(
        publisher.send_copy(3),
        Ok(0),
        "没有 subscriber 时 recipients=0"
    );

    eprintln!(
        "PUBSUB_DEAD_CLEANUP child_node={child_node_id} child_status={child_status:?} \
         cleanup_attempts={cleanup_attempts} cleanups={cleanups_total} retries={} liveness_after={liveness:?}",
        retries.load(Ordering::Relaxed)
    );
}
