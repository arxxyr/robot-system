//! 事件环独立验收：从双映射、跨圈及明确暂停交错检查整行归属。

use super::{TRACE_CAPACITY, TraceEvent, TraceKind, TracePage};

#[test]
fn arm_inside_real_normal_lease_waits_for_serial_boundary() {
    use robot_core::serial_services::{BusOpClass, LeasedPreemptProbe};
    use robot_core::time::{MockClock, MonoRawNs};
    use std::sync::Arc;

    const CHILD: &str = "ROBOT_TEST_BOUNDARY_CHILD";
    if std::env::var_os(CHILD).is_some() {
        super::install_from_env().unwrap();
        let path = std::env::var_os("ROBOT_TEST_TRACE_PATH").unwrap();
        let reader = TracePage::open(std::path::Path::new(&path)).unwrap();
        let ctrl = Arc::new(crate::hard_rt_ctrl::HardRtCtrlInner::new_serial(
            crate::hard_rt_ctrl::test_authority(false),
        ));
        let clock = Arc::new(MockClock::default());
        let probe = crate::serial_preempt::SerialPreemptProbe::new(clock, ctrl.clone());
        probe.set_context(
            BusOpClass::PeriodicRead,
            crate::serial_preempt::InstalledIntent::PeriodicRead,
            0,
            MonoRawNs(1_000_000),
            true,
        );
        let lease = probe.begin_leased(BusOpClass::PeriodicRead).unwrap();
        super::request_boundary(10072, super::SampleMode::ArmInflight, 1).unwrap();
        let during_lease = reader.armed_seq();
        probe.finish_leased(lease);
        let events = reader.drain(TRACE_CAPACITY);
        assert_eq!(during_lease, 0, "在途事务结束前不得切换采样 epoch");
        assert!(events.is_empty(), "旧 epoch 的 release 不得进入新样本");
        assert!(!super::serial_service_boundary(&ctrl, 2));
        assert_eq!(reader.armed_seq(), 10072);
        let ready = reader.boundary_snapshot().unwrap();
        assert_eq!(ready.receipt_seq, 10072);
        assert_eq!(ready.status, super::SampleStatus::Ready);
        let lease = probe.begin_leased(BusOpClass::PeriodicRead).unwrap();
        super::request_boundary(10073, super::SampleMode::Disarm, 3).unwrap();
        assert!(!super::serial_service_boundary(&ctrl, 4));
        assert_eq!(
            reader.armed_seq(),
            10072,
            "真实旧 lease 在途时 disarm 也须等待"
        );
        probe.finish_leased(lease);
        assert!(!super::serial_service_boundary(&ctrl, 5));
        assert_eq!(reader.armed_seq(), 0);
        let disarmed = reader.boundary_snapshot().unwrap();
        assert_eq!(disarmed.receipt_seq, 10073);
        assert_eq!(disarmed.status, super::SampleStatus::Disarmed);
        let pair = reader.drain(TRACE_CAPACITY);
        assert_eq!(pair.len(), 2);
        assert_eq!(pair[0].kind, TraceKind::NormalAcquire);
        assert_eq!(pair[1].kind, TraceKind::NormalRelease);
        assert!(
            pair.iter()
                .all(|event| event.sample_seq == 10072 && event.b == 1)
        );

        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("trace");
    let _page = TracePage::create(&path).unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "comm_fault_trace::acceptance::arm_inside_real_normal_lease_waits_for_serial_boundary",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .env("ROBOT_TEST_TRACE_PATH", &path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "真实采样接缝子进程失败：{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
}

fn event(index: u64) -> TraceEvent {
    TraceEvent {
        sample_seq: index + 1,
        stamp_ns: index * 17 + 3,
        kind: TraceKind::NormalAcquire,
        source: index % 6,
        normal_seq: index * 31 + 7,
        generation: u32::try_from(index + 19).unwrap(),
        a: index ^ 0x55aa,
        b: !index,
    }
}

#[test]
fn two_mappings_reuse_more_than_two_full_generations_without_stale_fields() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("trace");
    let writer = TracePage::create(&path).unwrap();
    let reader = TracePage::open(&path).unwrap();
    assert!(!reader.installed());
    writer.mark_installed();
    writer.arm(43);
    assert!(reader.installed());
    assert_eq!(reader.armed_seq(), 43);
    for batch in 0..12 {
        let start = batch * 2048;
        for index in start..start + 2048 {
            writer.record(event(index));
        }
        assert_eq!(
            reader.drain(2048),
            (start..start + 2048).map(event).collect::<Vec<_>>()
        );
        assert_eq!(reader.drain(1), []);
        assert_eq!(reader.dropped(), 0);
    }
    assert!(TracePage::create(&path).is_err(), "不能覆盖已存在的观测页");
}

#[test]
fn reserved_but_unpublished_first_slot_blocks_later_committed_slot() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("trace");
    let first = TracePage::create(&path).unwrap();
    let second = TracePage::open(&path).unwrap();
    let reader = TracePage::open(&path).unwrap();
    let (reserved_tx, reserved_rx) = std::sync::mpsc::sync_channel(0);
    let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(0);
    std::thread::scope(|scope| {
        let paused = scope.spawn(move || {
            first.record_after_reservation(event(0), || {
                reserved_tx.send(()).unwrap();
                resume_rx.recv().unwrap();
            });
        });
        reserved_rx.recv().unwrap();
        second.record(event(1));
        assert!(reader.drain(2).is_empty(), "未提交首槽之前不得越过读取后槽");
        assert_eq!(reader.dropped(), 0);
        resume_tx.send(()).unwrap();
        paused.join().unwrap();
    });
    assert_eq!(reader.drain(3), vec![event(0), event(1)]);
}

#[test]
fn full_8192_drops_only_the_new_event_and_keeps_old_rows() {
    assert_eq!(TRACE_CAPACITY, 8192, "固定协议容量不得由实现自证");
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("trace");
    let page = TracePage::create(&path).unwrap();
    for index in 0..8192 {
        page.record(event(index));
    }
    assert_eq!(page.dropped(), 0);
    page.record(event(8192));
    assert_eq!(page.dropped(), 1);
    assert_eq!(
        page.drain(usize::MAX),
        (0..8192).map(event).collect::<Vec<_>>()
    );
    page.record(event(8193));
    assert_eq!(page.drain(2), vec![event(8193)]);
    assert_eq!(page.dropped(), 1, "重用和arm均不能把丢弃计数清零");
    page.arm(999);
    assert_eq!(page.dropped(), 1);
}

#[test]
fn two_consumers_return_each_committed_row_once_across_reuse() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("trace");
    let writer = TracePage::create(&path).unwrap();
    let reader_a = TracePage::open(&path).unwrap();
    let reader_b = TracePage::open(&path).unwrap();
    for round in 0..3 {
        let start = round * 4096;
        for index in start..start + 4096 {
            writer.record(event(index));
        }
        let barrier = std::sync::Barrier::new(2);
        let mut rows = std::thread::scope(|scope| {
            let read = |page: &TracePage| {
                let mut rows = Vec::with_capacity(4096);
                barrier.wait();
                // 每轮有固定上限；空返回可能是另一consumer正在读，不意味着数据丢失。
                for _ in 0..4096 {
                    rows.extend(page.drain(7));
                }
                rows
            };
            let page_a = &reader_a;
            let page_b = &reader_b;
            let a = scope.spawn(move || read(page_a));
            let b = scope.spawn(move || read(page_b));
            let mut rows = a.join().unwrap();
            rows.extend(b.join().unwrap());
            rows
        });
        // 非阻塞 consumer 的争用失败也消耗调用次数；有限次竞争不保证清空。
        // 双方退出后单独收尾，再检查完整集合，避免把调度饥饿误报为事件丢失。
        rows.extend(writer.drain(TRACE_CAPACITY));
        rows.sort_unstable_by_key(|row| row.sample_seq);
        assert_eq!(rows.len(), 4096, "跨圈合并结果必须恰好包含本轮全部事件");
        for (row, index) in rows.iter().zip(start..start + 4096) {
            assert_eq!(*row, event(index), "事件字段或唯一归属不符，index={index}");
        }
        assert_eq!(writer.drain(1), []);
        assert_eq!(writer.dropped(), 0);
    }
}

#[test]
fn boundary_idle_deadline_is_fixed_and_failure_survives_disarm() {
    use super::{SampleFailure, SampleMode, SampleStatus};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("trace");
    let writer = TracePage::create(&path).unwrap();
    let reader = TracePage::open(&path).unwrap();
    let (ctrl, _clock) = crate::post_e_convergence_acceptance::healthy_ipc_control();
    writer
        .request_boundary(10, SampleMode::ArmIdle, 100)
        .unwrap();
    assert!(writer.service_boundary(&ctrl, 200));
    let ready = reader.boundary_snapshot().unwrap();
    assert_eq!(ready.request_seq, 10);
    assert_eq!(ready.receipt_seq, 10);
    assert_eq!(ready.mode, Some(SampleMode::ArmIdle));
    assert_eq!(ready.status, SampleStatus::Ready);
    assert_eq!(ready.request_deadline_ns, 1_000_000_100);
    assert_eq!(ready.ready_ns, 200);
    assert_eq!(ready.idle_deadline_ns, 1_000_000_200);
    assert!(writer.service_boundary(&ctrl, ready.idle_deadline_ns - 1));
    assert_eq!(
        reader.boundary_snapshot().unwrap().idle_deadline_ns,
        ready.idle_deadline_ns
    );
    assert!(!writer.service_boundary(&ctrl, ready.idle_deadline_ns));
    let failed = reader.boundary_snapshot().unwrap();
    assert_eq!(failed.failure_seq, 10);
    assert_eq!(failed.failure, Some(SampleFailure::IdleExpired));
    assert!(!failed.gate_active);
    assert_eq!(reader.armed_seq(), 10, "门到期不得截断尚待收集的 epoch");
    writer
        .request_boundary(11, SampleMode::Disarm, ready.idle_deadline_ns)
        .unwrap();
    assert!(!writer.service_boundary(&ctrl, ready.idle_deadline_ns + 1));
    let disarmed = reader.boundary_snapshot().unwrap();
    assert_eq!(disarmed.receipt_seq, 11);
    assert_eq!(disarmed.status, SampleStatus::Disarmed);
    assert_eq!(disarmed.armed_seq, 0);
    assert_eq!(disarmed.failure, Some(SampleFailure::IdleExpired));
    assert!(
        writer
            .request_boundary(12, SampleMode::ArmIdle, ready.idle_deadline_ns + 2)
            .is_err()
    );
    assert_eq!(
        reader.boundary_snapshot().unwrap().failure,
        Some(SampleFailure::IdleExpired)
    );
}

#[test]
fn boundary_request_cannot_overwrite_pending_or_reuse_a_completed_sequence() {
    use super::{SampleMode, SampleStatus};
    let directory = tempfile::tempdir().unwrap();
    let page = TracePage::create(&directory.path().join("trace")).unwrap();
    let ctrl = crate::hard_rt_ctrl::HardRtCtrlInner::new_serial(
        crate::hard_rt_ctrl::test_authority(false),
    );
    page.request_boundary(21, SampleMode::ArmInflight, 1)
        .unwrap();
    assert!(page.request_boundary(22, SampleMode::Disarm, 999).is_err());
    let pending = page.boundary_snapshot().unwrap();
    assert_eq!(pending.request_seq, 21);
    assert_eq!(pending.request_deadline_ns, 1_000_000_001);
    assert_eq!(page.armed_seq(), 0);
    page.service_boundary(&ctrl, 2);
    page.request_boundary(23, SampleMode::Disarm, 3).unwrap();
    page.service_boundary(&ctrl, 4);
    let completed = page.boundary_snapshot().unwrap();
    assert_eq!(completed.status, SampleStatus::Disarmed);
    assert_eq!(completed.receipt_seq, 23);
    assert!(page.request_boundary(23, SampleMode::Disarm, 5).is_err());
    assert!(page.request_boundary(20, SampleMode::Disarm, 5).is_err());
    assert_eq!(page.boundary_snapshot().unwrap().receipt_seq, 23);
}

#[test]
fn boundary_request_expiry_does_not_arm_after_a_late_serial_tick() {
    use super::{SampleFailure, SampleMode};
    let directory = tempfile::tempdir().unwrap();
    let page = TracePage::create(&directory.path().join("trace")).unwrap();
    let ctrl = crate::hard_rt_ctrl::HardRtCtrlInner::new_serial(
        crate::hard_rt_ctrl::test_authority(false),
    );
    page.request_boundary(31, SampleMode::ArmIdle, 17).unwrap();
    assert!(!page.service_boundary(&ctrl, 1_000_000_017));
    let result = page.boundary_snapshot().unwrap();
    assert_eq!(result.armed_seq, 0);
    assert!(!result.gate_active);
    assert_eq!(result.failure_seq, 31);
    assert_eq!(result.failure, Some(SampleFailure::BoundaryExpired));
}

#[test]
fn a_writing_disarm_cannot_hide_the_previous_idle_deadline() {
    use super::{SampleFailure, SampleGateRelease, SampleMode, SampleStatus};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("trace");
    let writer = TracePage::create(&path).unwrap();
    let reader = TracePage::open(&path).unwrap();
    let (ctrl, _clock) = crate::post_e_convergence_acceptance::healthy_ipc_control();
    writer
        .request_boundary(41, SampleMode::ArmIdle, 100)
        .unwrap();
    assert!(writer.service_boundary(&ctrl, 200));
    let deadline = reader.boundary_snapshot().unwrap().idle_deadline_ns;
    writer
        .request_boundary_after_writing(42, SampleMode::Disarm, deadline - 2, || {
            assert!(
                reader.boundary_snapshot().is_none(),
                "Writing 期间不能混读上一请求回执"
            );
            assert!(
                !writer.service_boundary(&ctrl, deadline),
                "新请求 Writing 不得阻止旧门按期释放"
            );
            assert_eq!(reader.armed_seq(), 41, "到期只撤 gate，不关闭旧事件 epoch");
        })
        .unwrap();
    assert!(!writer.service_boundary(&ctrl, deadline + 1));
    let result = reader.boundary_snapshot().unwrap();
    assert_eq!(result.receipt_seq, 42);
    assert_eq!(result.status, SampleStatus::Disarmed);
    assert_eq!(result.armed_seq, 0);
    assert_eq!(result.failure_seq, 41);
    assert_eq!(result.failure, Some(SampleFailure::IdleExpired));
    assert_eq!(result.gate_release, Some(SampleGateRelease::Expired));
}
