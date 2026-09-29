//! 真 SHM 镜像隔离回归；所有 topic 为测试独有，禁止连接正式运行时。
#![cfg(feature = "iceoryx2")]
#![allow(clippy::pedantic, clippy::nursery)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use iceoryx2::prelude::*;
use robot_msg::build_helpers::build_heartbeat;
use robot_msg::markers;
use robot_transport::iceoryx2::{
    Iceoryx2Publisher, ObserverMetadata, ObserverMonitor, ObserverSubscriber, TopicClass, channel,
    list_observers, list_observers_filtered, observer_subscriber, observer_topic,
};
use robot_transport::{LatestSubscriber, Publisher};

static NEXT: AtomicU64 = AtomicU64::new(0);
fn topic(label: &str) -> String {
    format!(
        "/observer_test/{}/{}/{label}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}
fn node() -> Node<ipc::Service> {
    NodeBuilder::new().create::<ipc::Service>().unwrap()
}
fn wait(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(Instant::now() < deadline, "有界测试等待超时");
        std::thread::sleep(Duration::from_millis(2));
    }
}
fn ready(monitor: &ObserverMonitor) {
    wait(|| {
        let s = monitor.snapshot();
        assert_eq!(s.initialization_failures, 0, "镜像初始化失败");
        s.ready
    });
}
fn publish(p: &mut Iceoryx2Publisher<markers::Heartbeat>, value: u64) {
    let body = build_heartbeat(2, value, 50.0, value * 1000);
    let mut w = p.loan(1024).unwrap();
    // 刻意重复原 seq，证明镜像 gap 不依赖 EStop 等原协议的 seq 递增。
    w.set_seq(1);
    w.set_source_id(2);
    w.set_stamp_ns(value * 1000);
    w.set_body_bytes(&body);
    assert_eq!(
        w.publish_typed().unwrap(),
        robot_transport::PublishReceipt::Iceoryx2 { recipients: 1 }
    );
}
fn receive(s: &mut ObserverSubscriber<markers::Heartbeat>) -> (ObserverMetadata, u64) {
    let mut result = None;
    wait(|| {
        if let Some(sample) = s.try_recv().unwrap() {
            let v = sample.view().unwrap();
            assert_eq!(v.header.seq(), 1);
            result = Some((sample.metadata(), v.body.get().seq()));
        }
        result.is_some()
    });
    result.unwrap()
}
fn assert_reaped(monitor: &ObserverMonitor) {
    let s = monitor.snapshot();
    assert!(s.finished && s.reaped, "真实 join 后才算回收: {s:?}");
    assert_eq!(s.pending_frames, 0);
    assert_eq!(s.pending_bytes, 0);
    assert_eq!(s.estimated_shm_bytes, 0);
    assert_eq!(s.join_failures, 0);
}

#[test]
fn observer_open_never_creates_either_service() {
    let n = node();
    let primary = topic("no_first_create");
    assert!(observer_subscriber::<markers::Heartbeat>(&n, &primary).is_err());
    for name in [&primary, &observer_topic(&primary).unwrap()] {
        assert!(
            !ipc::Service::does_exist(
                &ServiceName::new(name).unwrap(),
                n.config(),
                iceoryx2::service::messaging_pattern::MessagingPattern::PublishSubscribe,
            )
            .unwrap()
        );
    }
}

#[test]
fn discovery_filters_before_capacity_and_reports_truncation() {
    let n = node();
    let scope = topic("discovery");
    let selected = format!("{scope}/target/");
    let names = [
        format!("{scope}/other/heartbeat"),
        format!("{selected}one"),
        format!("{selected}two"),
    ];
    let publishers: Vec<_> = names
        .iter()
        .map(|name| {
            robot_transport::iceoryx2::publisher::<markers::Heartbeat>(
                &n,
                name,
                TopicClass::Heartbeat,
            )
            .unwrap()
        })
        .collect();
    let monitors: Vec<_> = publishers
        .iter()
        .map(Iceoryx2Publisher::observer_monitor)
        .collect();
    for monitor in &monitors {
        ready(monitor);
    }
    let exact = list_observers_filtered(n.config(), 1, |p| p == names[1]).unwrap();
    assert_eq!(exact.topics.len(), 1);
    assert_eq!(exact.topics[0].primary_topic, names[1]);
    assert!(!exact.capacity_reached);
    let bounded = list_observers_filtered(n.config(), 1, |p| p.starts_with(&selected)).unwrap();
    assert_eq!(bounded.topics.len(), 1);
    assert!(bounded.capacity_reached);
    assert!(bounded.topics[0].primary_topic.starts_with(&selected));
    let probe = list_observers_filtered(n.config(), 0, |p| p.starts_with(&selected)).unwrap();
    assert!(probe.topics.is_empty() && probe.capacity_reached);
    let complete = list_observers_filtered(n.config(), 2, |p| p.starts_with(&selected)).unwrap();
    assert_eq!(complete.topics.len(), 2);
    assert!(!complete.capacity_reached);
    assert!(list_observers_filtered(n.config(), 1025, |_| true).is_err());
    drop(publishers);
    for monitor in &monitors {
        assert_reaped(monitor);
    }
}

#[test]
fn two_observers_fifo_and_paused_reader_do_not_take_primary_command_slot() {
    let n = node();
    let primary = topic("single_primary");
    let (mut p, mut control) =
        channel::<markers::Heartbeat>(&n, &primary, TopicClass::S0Command).unwrap();
    let monitor = p.observer_monitor();
    ready(&monitor);
    // 主 QoS 仍是一个 subscriber；观测者只能进镜像。
    assert!(
        robot_transport::iceoryx2::subscriber::<markers::Heartbeat>(
            &n,
            &primary,
            TopicClass::S0Command
        )
        .is_err()
    );
    let mut a = observer_subscriber::<markers::Heartbeat>(&n, &primary).unwrap();
    let mut b = observer_subscriber::<markers::Heartbeat>(&n, &primary).unwrap();
    for value in 1..=4 {
        publish(&mut p, value);
        assert_eq!(
            control
                .latest()
                .unwrap()
                .unwrap()
                .view()
                .unwrap()
                .body
                .get()
                .seq(),
            value
        );
        wait(|| monitor.snapshot().published == value);
        let (ma, va) = receive(&mut a);
        let (mb, vb) = receive(&mut b);
        assert_eq!((va, vb), (value, value));
        assert_eq!(ma, mb);
        assert_eq!(ma.observer_seq, value);
    }
    // 保留 b 但不读（模拟暂停）；a 继续接收，主控制每次收到原消息。
    for value in 5..=40 {
        publish(&mut p, value);
        assert_eq!(
            control
                .latest()
                .unwrap()
                .unwrap()
                .view()
                .unwrap()
                .body
                .get()
                .seq(),
            value
        );
        wait(|| monitor.snapshot().published == value);
        assert_eq!(receive(&mut a).1, value);
    }
    let (late, _) = receive(&mut b);
    assert!(
        late.observer_seq > 5,
        "暂停读者可见序号缺口，不能阻塞主通道"
    );
    drop(b);
    publish(&mut p, 41);
    assert_eq!(
        control
            .latest()
            .unwrap()
            .unwrap()
            .view()
            .unwrap()
            .body
            .get()
            .seq(),
        41
    );
    wait(|| monitor.snapshot().published == 41);
    assert_eq!(receive(&mut a).1, 41);
    drop(p);
    assert_reaped(&monitor);
}

#[test]
fn late_observer_gets_history_and_new_primary_has_new_instance() {
    let n = node();
    let primary = topic("restart");
    let (mut p, mut control) =
        channel::<markers::Heartbeat>(&n, &primary, TopicClass::Heartbeat).unwrap();
    let first = p.observer_monitor();
    ready(&first);
    publish(&mut p, 1);
    control.latest().unwrap();
    wait(|| first.snapshot().published == 1);
    let mut sub = observer_subscriber::<markers::Heartbeat>(&n, &primary).unwrap();
    let (old, _) = receive(&mut sub);
    assert!(
        list_observers(n.config())
            .unwrap()
            .iter()
            .any(|t| t.primary_topic == primary && t.schema_id == 0x301)
    );
    drop(p);
    assert_reaped(&first);
    let mut p = robot_transport::iceoryx2::publisher::<markers::Heartbeat>(
        &n,
        &primary,
        TopicClass::Heartbeat,
    )
    .unwrap();
    let second = p.observer_monitor();
    ready(&second);
    publish(&mut p, 2);
    wait(|| second.snapshot().published == 1);
    let (new, value) = receive(&mut sub);
    assert_eq!(value, 2);
    assert_eq!(new.observer_seq, 1);
    assert_ne!(old.publisher_instance_id, new.publisher_instance_id);
    drop(p);
    assert_reaped(&second);
}

#[test]
fn mirror_creation_failure_does_not_change_primary_result() {
    let n = node();
    let primary = topic("incompatible_mirror");
    let mirror = observer_topic(&primary).unwrap();
    // 提前放置不匹配属性的镜像名，不能回退创建别的正式服务。
    let blocked = n
        .service_builder(&ServiceName::new(&mirror).unwrap())
        .publish_subscribe::<[u8]>()
        .create()
        .unwrap();
    let _blocked_publisher = blocked.publisher_builder().create().unwrap();
    let (mut p, mut control) =
        channel::<markers::Heartbeat>(&n, &primary, TopicClass::S0Command).unwrap();
    let monitor = p.observer_monitor();
    wait(|| monitor.snapshot().finished);
    assert_eq!(monitor.snapshot().initialization_failures, 1);
    for value in 1..=3 {
        publish(&mut p, value);
        assert_eq!(
            control
                .latest()
                .unwrap()
                .unwrap()
                .view()
                .unwrap()
                .body
                .get()
                .seq(),
            value
        );
    }
    assert_eq!(monitor.snapshot().dropped_frames, 3);
    drop(p);
    assert_reaped(&monitor);
}

#[test]
fn holding_mirror_loans_and_reader_restart_preserve_primary() {
    let n = node();
    let primary = topic("holding");
    let (mut p, mut control) =
        channel::<markers::Heartbeat>(&n, &primary, TopicClass::S0Command).unwrap();
    let monitor = p.observer_monitor();
    ready(&monitor);
    let mut sub = observer_subscriber::<markers::Heartbeat>(&n, &primary).unwrap();
    publish(&mut p, 1);
    control.latest().unwrap();
    wait(|| monitor.snapshot().published == 1);
    let mut held = None;
    wait(|| {
        held = sub.try_recv().unwrap();
        held.is_some()
    });
    for value in 2..=24 {
        publish(&mut p, value);
        assert_eq!(
            control
                .latest()
                .unwrap()
                .unwrap()
                .view()
                .unwrap()
                .body
                .get()
                .seq(),
            value
        );
        wait(|| monitor.snapshot().published == value);
    }
    assert_eq!(held.as_ref().unwrap().view().unwrap().body.get().seq(), 1);
    drop(sub);
    drop(held);
    let mut restarted = observer_subscriber::<markers::Heartbeat>(&n, &primary).unwrap();
    let (_, value) = receive(&mut restarted);
    assert!(value >= 23);
    drop(p);
    assert_reaped(&monitor);
}

#[test]
fn dropping_during_startup_requests_stop_and_reaps() {
    let n = node();
    let primary = topic("early_stop");
    let p = robot_transport::iceoryx2::publisher::<markers::Heartbeat>(
        &n,
        &primary,
        TopicClass::Heartbeat,
    )
    .unwrap();
    let monitor = p.observer_monitor();
    drop(p);
    assert_reaped(&monitor);
}

#[test]
fn primary_preflight_failure_never_enters_mirror_queue() {
    let n = node();
    let primary = topic("primary_failure");
    let (mut p, mut control) =
        channel::<markers::Heartbeat>(&n, &primary, TopicClass::Heartbeat).unwrap();
    let monitor = p.observer_monitor();
    ready(&monitor);
    let mut sub = observer_subscriber::<markers::Heartbeat>(&n, &primary).unwrap();
    let body = build_heartbeat(2, 1, 50.0, 1000);
    let mut writer = p.loan(1).unwrap();
    writer.set_body_bytes(&body);
    assert!(matches!(
        writer.publish_typed(),
        Err(robot_transport::PublishFailure::BeforeHistory(_))
    ));
    assert_eq!(monitor.snapshot().published, 0);
    assert_eq!(monitor.snapshot().pending_frames, 0);
    assert_eq!(monitor.snapshot().dropped_frames, 0);
    publish(&mut p, 1);
    control.latest().unwrap();
    assert_eq!(receive(&mut sub).0.observer_seq, 1);
    drop(p);
    assert_reaped(&monitor);
}

/// 单独子进程入口：仅在父测试指定唯一镜像 topic 后使用。
#[test]
#[ignore = "仅供 killed_reader 回归启动子进程"]
fn observer_child_paused_fixture() {
    let primary = std::env::var("ROBOT_OBSERVER_CHILD_TOPIC").unwrap();
    let ready_file = std::env::var_os("ROBOT_OBSERVER_CHILD_READY").unwrap();
    let n = node();
    let _subscriber = observer_subscriber::<markers::Heartbeat>(&n, &primary).unwrap();
    std::fs::write(ready_file, b"ready").unwrap();
    loop {
        std::thread::park();
    }
}

struct ChildGuard(std::process::Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn killed_reader_never_changes_primary_command_delivery() {
    let n = node();
    let primary = topic("killed_reader");
    let (mut p, mut control) =
        channel::<markers::Heartbeat>(&n, &primary, TopicClass::S0Command).unwrap();
    let monitor = p.observer_monitor();
    ready(&monitor);
    let ready_path = std::env::temp_dir().join(format!(
        "robot_observer_ready_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "observer_child_paused_fixture",
            "--ignored",
            "--test-threads=1",
        ])
        .env("ROBOT_OBSERVER_CHILD_TOPIC", &primary)
        .env("ROBOT_OBSERVER_CHILD_READY", &ready_path)
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut child = ChildGuard(child);
    wait(|| {
        assert!(child.0.try_wait().unwrap().is_none(), "reader 在就绪前退出");
        ready_path.exists()
    });
    for value in 1..=12 {
        publish(&mut p, value);
        assert_eq!(
            control
                .latest()
                .unwrap()
                .unwrap()
                .view()
                .unwrap()
                .body
                .get()
                .seq(),
            value
        );
        wait(|| monitor.snapshot().pending_frames == 0);
    }
    child.0.kill().unwrap();
    assert!(!child.0.wait().unwrap().success());
    for value in 13..=24 {
        publish(&mut p, value);
        assert_eq!(
            control
                .latest()
                .unwrap()
                .unwrap()
                .view()
                .unwrap()
                .body
                .get()
                .seq(),
            value
        );
        wait(|| monitor.snapshot().pending_frames == 0);
    }
    drop(p);
    assert_reaped(&monitor);
    std::fs::remove_file(ready_path).unwrap();
}
