//! 事件环独立验收：从双映射、跨圈及明确暂停交错检查整行归属。

use super::{TRACE_CAPACITY, TraceEvent, TraceKind, TracePage};

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
