//! P-S1/P-L1 公开原语验收；不接生产线程或真实硬件。

use robot_s0::event_ingress::{
    ControlEvent, HighClass, HighQueue, HighWork, NormalQueue, NormalWork, RrEvent,
};
use robot_s0::ipc_thread::{ClearEStopRrRequest, SetEnabledRrRequest};
use robot_s0::rr_admission::{
    CheckedLimits, ClassCaps, ClassCounts, LimitsError, PeerLedger, RrRequestClass, SlotMismatch,
};
use robot_s0::watchdog::{Freshness, is_overdue, judge_heartbeat_timeout};
use robot_transport::iceoryx2::RrStaticLimits;

#[cfg(feature = "rt_guard")]
#[global_allocator]
static ALLOC_GUARD: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

fn limits(c: usize) -> CheckedLimits {
    CheckedLimits::try_from_limits(&RrStaticLimits { c, l: 4 }).unwrap()
}

fn set(id: usize, dry_run: bool, enabled: bool) -> RrEvent {
    RrEvent::SetEnabled(SetEnabledRrRequest {
        request_id: format!("set-{id}"),
        enabled,
        dry_run,
        deadline_ns: u64::try_from(id).unwrap(),
        audit_reason: "公开准入验收".into(),
    })
}

fn clear(id: usize, dry_run: bool) -> RrEvent {
    RrEvent::ClearEStop(ClearEStopRrRequest {
        request_id: format!("clear-{id}"),
        dry_run,
        confirmation: "明确确认".into(),
        audit_reason: "公开准入验收".into(),
        deadline_ns: u64::try_from(id).unwrap(),
    })
}

#[test]
fn public_api_cross_ledger_slot_returned_unchanged() {
    let mut first = PeerLedger::<u32>::new(&limits(8)).unwrap();
    let mut second = PeerLedger::<u32>::new(&limits(8)).unwrap();
    assert_ne!(first.id(), second.id());
    let slot = first.admit(37, RrRequestClass::Restore).unwrap();
    let first_before = first.counts();
    let second_before = second.counts();
    let SlotMismatch(returned) = second.release(slot).unwrap_err();
    assert_eq!(returned.peer(), 37);
    assert_eq!(returned.class(), RrRequestClass::Restore);
    assert_eq!(first.counts(), first_before);
    assert_eq!(second.counts(), second_before);
    first.release(returned).unwrap();
    assert_eq!(first.peer_counts(37), None);
    assert_eq!(first.counts(), ClassCounts::default());
}

#[test]
fn public_api_real_sources_high_then_normal_order() {
    let mut high = HighQueue::new();
    let mut normal = NormalQueue::with_capacity(NormalQueue::capacity_for(&limits(8)));
    high.admit(HighWork::from_rr(clear(1, false)).unwrap())
        .unwrap();
    normal
        .admit(NormalWork::from_rr(set(2, true, true)).unwrap())
        .unwrap();
    high.admit(HighWork::from_rr(set(3, false, false)).unwrap())
        .unwrap();
    high.admit(HighWork::control(ControlEvent::Stop)).unwrap();
    high.admit(HighWork::from_rr(set(4, false, true)).unwrap())
        .unwrap();
    normal
        .admit(NormalWork::from_rr(clear(5, true)).unwrap())
        .unwrap();
    let mut actual = Vec::new();
    while let Some(work) = high.pop() {
        match work {
            HighWork::LiveDisable(request) | HighWork::LiveEnable(request) => {
                actual.push(request.request_id);
            }
            HighWork::Control(ControlEvent::Stop) => actual.push("stop".into()),
            HighWork::LiveClear(request) => actual.push(request.request_id),
            HighWork::Control(other) => panic!("非预期控制事件 {other:?}"),
        }
    }
    while let Some(work) = normal.pop() {
        match work {
            NormalWork::DryRunSet(request) => actual.push(request.request_id),
            NormalWork::DryRunClear(request) => actual.push(request.request_id),
        }
    }
    assert_eq!(
        actual,
        ["set-3", "stop", "clear-1", "set-4", "set-2", "clear-5"]
    );
}

#[test]
fn public_api_heartbeat_freshness_equal_only() {
    for (latest, expected) in [
        (
            99,
            Freshness::Stale {
                latest_hb_ns: 99,
                judged_hb_ns: 100,
            },
        ),
        (100, Freshness::Valid),
        (
            101,
            Freshness::Stale {
                latest_hb_ns: 101,
                judged_hb_ns: 100,
            },
        ),
    ] {
        assert_eq!(judge_heartbeat_timeout(latest, 100), expected);
    }
    assert!(!is_overdue(110, 100, 10));
    assert!(is_overdue(111, 100, 10));
    assert!(!is_overdue(99, 100, 0));
}

#[test]
fn public_api_checked_limits_matrix() {
    for (c, l, expected) in [
        (0, 4, LimitsError::CTooSmall { c: 0 }),
        (0, usize::MAX, LimitsError::CTooSmall { c: 0 }),
        (
            16,
            usize::MAX,
            LimitsError::Overflow {
                c: 16,
                l: usize::MAX,
            },
        ),
        (usize::MAX, 4, LimitsError::CTooLarge { c: usize::MAX }),
        (
            16,
            5,
            LimitsError::ProductTooLarge {
                c: 16,
                l: 5,
                product: 80,
            },
        ),
        (8, 2, LimitsError::LTooSmall { l: 2 }),
        (17, 4, LimitsError::CTooLarge { c: 17 }),
    ] {
        assert_eq!(
            CheckedLimits::try_from_limits(&RrStaticLimits { c, l }),
            Err(expected)
        );
    }
    assert_eq!(
        ClassCaps::from_checked(&limits(16)),
        ClassCaps {
            restore: 16,
            lower: 16,
            normal: 32,
            total: 64
        }
    );
    assert_eq!(NormalQueue::capacity_for(&limits(8)), 32);
    assert_eq!(NormalQueue::capacity_for(&limits(16)), 64);
}

#[test]
fn alloc_guard_admit_release_and_prefilled_queues_no_alloc() {
    let mut ledger = PeerLedger::<u32>::new(&limits(16)).unwrap();
    let mut high = HighQueue::new();
    let mut normal = NormalQueue::with_capacity(NormalQueue::capacity_for(&limits(16)));
    let mut lower: [Option<HighWork>; 32] = std::array::from_fn(|id| {
        Some(if id % 2 == 0 {
            HighWork::control(ControlEvent::Stop)
        } else {
            HighWork::from_rr(set(id, false, false)).unwrap()
        })
    });
    let mut restore: [Option<HighWork>; 32] = std::array::from_fn(|id| {
        Some(
            HighWork::from_rr(if id % 2 == 0 {
                clear(id, false)
            } else {
                set(id, false, true)
            })
            .unwrap(),
        )
    });
    let mut ordinary: [Option<NormalWork>; 64] = std::array::from_fn(|id| {
        Some(
            NormalWork::from_rr(if id % 2 == 0 {
                clear(id, true)
            } else {
                set(id, true, true)
            })
            .unwrap(),
        )
    });
    let mut slots: [_; 64] = std::array::from_fn(|_| None);
    // 真实堆载荷都在 scope 外构造；出队后搬回数组，scope 外再析构字符串。
    robot_core::rt_entry!({
        for _ in 0..3 {
            for (peer, chunk) in slots.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                for (output, class) in chunk.iter_mut().zip([
                    RrRequestClass::Normal,
                    RrRequestClass::Normal,
                    RrRequestClass::Lower,
                    RrRequestClass::Restore,
                ]) {
                    *output = Some(ledger.admit(u32::try_from(peer).unwrap(), class).unwrap());
                }
            }
            assert!(ledger.admit(17, RrRequestClass::Normal).is_err());
            for slot in &mut slots {
                ledger.release(slot.take().unwrap()).unwrap();
            }
            for (batch, class) in [
                (&mut lower, HighClass::Lower),
                (&mut restore, HighClass::Restore),
            ] {
                for input in &mut *batch {
                    high.admit(input.take().unwrap()).unwrap();
                }
                assert_eq!(high.len(), 32);
                let overflow = high
                    .admit(HighWork::control(ControlEvent::Stop))
                    .unwrap_err();
                assert!(matches!(overflow, HighWork::Control(ControlEvent::Stop)));
                for output in &mut *batch {
                    *output = high.pop();
                    assert_eq!(output.as_ref().unwrap().class(), class);
                }
            }
            for input in &mut ordinary {
                normal.admit(input.take().unwrap()).unwrap();
            }
            assert_eq!(normal.len(), 64);
            for output in &mut ordinary {
                *output = normal.pop();
            }
        }
    });
    assert_eq!(ledger.counts(), ClassCounts::default());
    assert!(high.is_empty() && normal.is_empty());
    assert!(
        lower.iter().all(Option::is_some)
            && restore.iter().all(Option::is_some)
            && ordinary.iter().all(Option::is_some)
    );
}

#[cfg(all(feature = "rt_guard", unix))]
mod alloc_guard {
    use robot_core::time::{RobotClock, SystemClock};
    use std::io::Write;
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Child, Command, ExitStatus, Stdio};
    use std::time::Duration;

    const CHILD_ENV: &str = "ROBOT_PRIORITY_ALLOC_GUARD_CHILD";
    const MARKER: &[u8] = b"priority_alloc_guard entered_scope\n";

    struct SupervisedChild(Option<Child>);
    impl SupervisedChild {
        fn wait_until(&mut self, deadline: u64) -> Option<ExitStatus> {
            loop {
                if let Some(status) = self
                    .0
                    .as_mut()
                    .unwrap()
                    .try_wait()
                    .expect("读取子进程真实状态")
                {
                    self.0.take();
                    return Some(status);
                }
                if SystemClock.now().0 >= deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        fn kill_and_reap(&mut self) -> ExitStatus {
            let _ = self.0.as_mut().unwrap().kill();
            self.wait_until(SystemClock.now().0 + 5_000_000_000)
                .expect("SIGKILL 后必须在有界监督内回收子进程")
        }
    }
    impl Drop for SupervisedChild {
        fn drop(&mut self) {
            if self.0.is_some() {
                let _ = self.kill_and_reap();
            }
        }
    }

    fn violate() -> ! {
        let core_limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: 栈上 rlimit 指针有效；只禁止此反证子进程生成 core。
        assert_eq!(
            unsafe { libc::setrlimit(libc::RLIMIT_CORE, &raw const core_limit) },
            0
        );
        let escaped = robot_core::rt_entry!({
            // SAFETY: 静态字节切片有效，直接写入避免进入标记自行分配。
            let written =
                unsafe { libc::write(libc::STDERR_FILENO, MARKER.as_ptr().cast(), MARKER.len()) };
            assert_eq!(usize::try_from(written).ok(), Some(MARKER.len()));
            std::hint::black_box(Box::new(std::hint::black_box(42_u64)))
        });
        drop(escaped);
        std::process::exit(73);
    }

    #[test]
    fn installed_alloc_disabler_aborts_allocation_inside_rt_entry() {
        if std::env::var_os(CHILD_ENV).is_some() {
            violate();
        }
        let stderr = tempfile::NamedTempFile::new().unwrap();
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "alloc_guard::installed_alloc_disabler_aborts_allocation_inside_rt_entry",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_ENV, "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr.reopen().unwrap())
            .spawn()
            .expect("启动分配违规子进程");
        let mut child = SupervisedChild(Some(child));
        let Some(status) = child.wait_until(SystemClock.now().0 + 20_000_000_000) else {
            let status = child.kill_and_reap();
            panic!("分配反证超时，已实际回收：{status}");
        };
        let raw = std::fs::read(stderr.path()).unwrap();
        assert_eq!(
            status.signal(),
            Some(libc::SIGABRT),
            "真实退出状态={status} stderr={}",
            String::from_utf8_lossy(&raw)
        );
        assert_eq!(
            raw.windows(MARKER.len())
                .filter(|bytes| *bytes == MARKER)
                .count(),
            1
        );
        // 直接写 libtest 捕获之外，正式 raw 可独立核验终止原因与实际回收。
        writeln!(
            std::io::stdout().lock(),
            "priority_guard_verified signal={} scope_marker=1 child_reaped=true",
            status.signal().unwrap()
        )
        .unwrap();
    }
}
