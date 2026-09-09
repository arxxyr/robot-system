//! S48 A3b：快照发布路径的真实 allocation guard（首次发布 / 稳态 / 耗尽 / codec）。
//!
//! 本二进制安装真实 `assert_no_alloc::AllocDisabler`；先由独立子进程证明 scope 内的
//! 堆分配实际 abort，再让 `RtSnapshot` 与 `S0StateSnapshot` 两条生产 codec 的
//! publish / load / encode / decode 在同一 scope 内连续执行。构造（`Box` / `Arc`
//! 分配）在 scope 之外。
//!
//! ```bash
//! cargo +nightly test --locked -p robot-s0 --features "rt_guard test-comm-fault-hook" \
//!     --test snapshot_alloc_guard -- --test-threads=1
//! cargo +nightly test --locked --release -p robot-s0 --features "rt_guard test-comm-fault-hook" \
//!     --test snapshot_alloc_guard -- --test-threads=1
//! ```

#![cfg(all(unix, feature = "rt_guard", feature = "test-comm-fault-hook"))]
#![allow(clippy::doc_markdown)]

use std::hint::black_box;

use robot_core::adapter::{MAX_JOINTS, RtSnapshot};
use robot_core::executor_ctrl::OutputSafetyState;
use robot_core::time::{RobotClock, SystemClock};
use robot_s0::state_machine::{
    ControlLink, EnabledSub, FaultCode, Guards, OutputMode, S0State, S0StateSnapshot,
};
use robot_transport::{
    MAX_PUBLISHED_VERSION, RtSnapshotPublisher, RtSnapshotReader, SnapshotPublishError,
    SnapshotValue, SnapshotWords,
};

#[global_allocator]
static ALLOC_GUARD: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

const CHILD_ENV: &str = "ROBOT_TEST_SNAPSHOT_ALLOC_CHILD";
const SCOPE_MARKER: &[u8] = b"snapshot_alloc_guard entered_scope\n";

fn write_marker() {
    // SAFETY: 静态字节切片有效；直接写 stderr 避免标记本身触发堆分配。
    let written = unsafe {
        libc::write(
            libc::STDERR_FILENO,
            SCOPE_MARKER.as_ptr().cast(),
            SCOPE_MARKER.len(),
        )
    };
    assert_eq!(usize::try_from(written).ok(), Some(SCOPE_MARKER.len()));
}

fn allocate_inside_real_scope() -> ! {
    let core_limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: 指向当前栈上有效 rlimit；只限制本测试子进程。
    assert_eq!(
        unsafe { libc::setrlimit(libc::RLIMIT_CORE, &raw const core_limit) },
        0
    );
    let layout = std::alloc::Layout::from_size_align(64, 8).unwrap();
    let pointer = assert_no_alloc::assert_no_alloc(|| {
        write_marker();
        // SAFETY: layout 合法；若守卫失效则在 scope 外按同一 layout 释放。
        black_box(unsafe { std::alloc::alloc(black_box(layout)) })
    });
    if !pointer.is_null() {
        // SAFETY: pointer 来自上面同一 layout 的分配且尚未释放。
        unsafe { std::alloc::dealloc(pointer, layout) };
    }
    std::process::exit(73);
}

#[test]
fn installed_alloc_disabler_aborts_allocation_inside_the_scope() {
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Command, Stdio};

    if std::env::var(CHILD_ENV).as_deref() == Ok("1") {
        allocate_inside_real_scope();
    }
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "installed_alloc_disabler_aborts_allocation_inside_the_scope",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("启动分配违规子进程");
    let deadline = SystemClock.now().0 + 20_000_000_000;
    while child.try_wait().expect("读取子进程状态").is_none() {
        if SystemClock.now().0 >= deadline {
            child.kill().expect("终止未退场的子进程");
            let output = child.wait_with_output().unwrap();
            panic!("分配反例超时：{}", String::from_utf8_lossy(&output.stderr));
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let output = child.wait_with_output().unwrap();
    assert_eq!(
        output.status.signal(),
        Some(libc::SIGABRT),
        "已安装的 AllocDisabler 必须实际 abort：status={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output
            .stderr
            .windows(SCOPE_MARKER.len())
            .filter(|window| *window == SCOPE_MARKER)
            .count(),
        1
    );
}

fn rt_sample(seed: u64) -> RtSnapshot {
    let mut snapshot = RtSnapshot::default();
    for index in 0..MAX_JOINTS {
        let mixed = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ u64::try_from(index).unwrap();
        let bits = (mixed & 0xffff_ffff) as u32;
        snapshot.state.positions[index] = f32::from_bits(bits);
        snapshot.state.velocities[index] = f32::from_bits(bits ^ 0x8000_0000);
        snapshot.state.torques[index] = f32::from_bits(bits | 0x7f80_0000);
    }
    snapshot.state.stamp_ns = seed;
    snapshot.state.joint_count = (seed & 0xff) as u8;
    snapshot.tick_seq = seed.wrapping_mul(3);
    snapshot.tick_deadline_ns = seed.wrapping_mul(5);
    snapshot
}

fn state_sample(seed: u64) -> S0StateSnapshot {
    let state = match seed % 4 {
        0 => S0State::Safe,
        1 => S0State::Enabled {
            sub: EnabledSub::CtrlLostZero,
            control_link: ControlLink::Lost,
            output_mode: OutputMode::Hold,
        },
        2 => S0State::Fault(FaultCode::SafetyStopFailed),
        _ => S0State::Stopping,
    };
    S0StateSnapshot::new(
        state,
        Guards {
            estop_latched: seed & 1 == 1,
            heartbeat_ok: seed & 2 == 2,
            command_mode: robot_core::adapter::ControlMode::Torque,
            on_timeout_hint: EnabledSub::CtrlLostHold,
            ..Guards::default()
        },
        seed,
    )
}

fn rt_equal(a: &RtSnapshot, b: &RtSnapshot) -> bool {
    let bits = |x: &[f32; MAX_JOINTS], y: &[f32; MAX_JOINTS]| {
        x.iter().zip(y).all(|(p, q)| p.to_bits() == q.to_bits())
    };
    bits(&a.state.positions, &b.state.positions)
        && bits(&a.state.velocities, &b.state.velocities)
        && bits(&a.state.torques, &b.state.torques)
        && a.state.stamp_ns == b.state.stamp_ns
        && a.state.joint_count == b.state.joint_count
        && a.tick_seq == b.tick_seq
        && a.tick_deadline_ns == b.tick_deadline_ns
}

/// scope 内只记录 Copy 判定，断言放在 scope 外。
#[derive(Clone, Copy, Debug, Default)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "每个字段是一条路径在无分配 scope 内的判定"
)]
struct GuardReport {
    first_publish_ok: bool,
    steady_publishes_ok: bool,
    steady_reads_consistent: bool,
    exhausted_reported: bool,
    exhausted_keeps_last: bool,
    codec_roundtrip_ok: bool,
    state_first_publish_ok: bool,
    state_steady_ok: bool,
    state_exhausted_reported: bool,
    state_codec_roundtrip_ok: bool,
    version_rt: u64,
    version_state: u64,
}

struct Fixture {
    fresh_rt: RtSnapshotPublisher<RtSnapshot>,
    fresh_rt_reader: RtSnapshotReader<RtSnapshot>,
    near_max_rt: RtSnapshotPublisher<RtSnapshot>,
    near_max_rt_reader: RtSnapshotReader<RtSnapshot>,
    fresh_state: RtSnapshotPublisher<S0StateSnapshot>,
    fresh_state_reader: RtSnapshotReader<S0StateSnapshot>,
    near_max_state: RtSnapshotPublisher<S0StateSnapshot>,
    near_max_state_reader: RtSnapshotReader<S0StateSnapshot>,
}

#[allow(
    clippy::too_many_lines,
    reason = "全部快照热路径必须在同一个无分配 scope 内连续执行"
)]
fn run_hot_paths(fixture: &mut Fixture) -> GuardReport {
    // 首次发布（版本 0 → 2）。
    let first_publish_ok =
        fixture.fresh_rt.publish(&rt_sample(1)).is_ok() && fixture.fresh_rt_reader.version() == 2;
    let mut report = GuardReport {
        first_publish_ok,
        ..GuardReport::default()
    };

    // 稳态：连续发布并读取，读到的值必须逐位等于最近一次发布。
    let mut steady_ok = true;
    let mut reads_ok = true;
    for seed in 2..34u64 {
        let sample = rt_sample(seed);
        steady_ok &= fixture.fresh_rt.publish(&sample).is_ok();
        reads_ok &= rt_equal(&fixture.fresh_rt_reader.load(), &sample);
    }
    report.steady_publishes_ok = steady_ok;
    report.steady_reads_consistent = reads_ok;

    // 耗尽：MAX-2 → 一次成功到 MAX，再发布返回错误且上一快照保留。
    let last = rt_sample(99);
    let reached_max = fixture.near_max_rt.publish(&last).is_ok()
        && fixture.near_max_rt_reader.version() == MAX_PUBLISHED_VERSION;
    let refused =
        fixture.near_max_rt.publish(&rt_sample(100)) == Err(SnapshotPublishError::VersionExhausted);
    report.exhausted_reported = reached_max && refused;
    report.exhausted_keeps_last = rt_equal(&fixture.near_max_rt_reader.load(), &last)
        && fixture.near_max_rt_reader.version() == MAX_PUBLISHED_VERSION;

    // codec 直接往返（栈上 Words）。
    let mut words = <RtSnapshot as SnapshotValue>::Words::zeroed();
    let sample = rt_sample(7);
    sample.encode(&mut words);
    report.codec_roundtrip_ok = rt_equal(&RtSnapshot::decode(&words), &sample)
        && words.as_words().len() == <<RtSnapshot as SnapshotValue>::Words as SnapshotWords>::LEN;

    // S0StateSnapshot 同样四条路径。
    report.state_first_publish_ok = fixture.fresh_state.publish(&state_sample(1)).is_ok();
    let mut state_ok = true;
    for seed in 2..18u64 {
        let sample = state_sample(seed);
        state_ok &= fixture.fresh_state.publish(&sample).is_ok()
            && fixture.fresh_state_reader.load() == sample;
    }
    report.state_steady_ok = state_ok;
    let last_state = state_sample(5);
    report.state_exhausted_reported = fixture.near_max_state.publish(&last_state).is_ok()
        && fixture.near_max_state.publish(&state_sample(6))
            == Err(SnapshotPublishError::VersionExhausted)
        && fixture.near_max_state_reader.load() == last_state;
    let mut state_words = <S0StateSnapshot as SnapshotValue>::Words::zeroed();
    let sample = state_sample(2);
    sample.encode(&mut state_words);
    report.state_codec_roundtrip_ok = S0StateSnapshot::decode(&state_words) == sample;

    report.version_rt = fixture.fresh_rt_reader.version();
    report.version_state = fixture.fresh_state_reader.version();
    report
}

#[test]
fn snapshot_first_steady_exhausted_and_codec_paths_run_without_allocation() {
    let (fresh_rt, fresh_rt_reader) = RtSnapshotPublisher::new(RtSnapshot::default());
    let (near_max_rt, near_max_rt_reader) =
        RtSnapshotPublisher::new_with_version(RtSnapshot::default(), MAX_PUBLISHED_VERSION - 2)
            .unwrap();
    let (fresh_state, fresh_state_reader) = RtSnapshotPublisher::new(S0StateSnapshot::default());
    let (near_max_state, near_max_state_reader) = RtSnapshotPublisher::new_with_version(
        S0StateSnapshot::default(),
        MAX_PUBLISHED_VERSION - 2,
    )
    .unwrap();
    let mut fixture = Fixture {
        fresh_rt,
        fresh_rt_reader,
        near_max_rt,
        near_max_rt_reader,
        fresh_state,
        fresh_state_reader,
        near_max_state,
        near_max_state_reader,
    };
    let _ = OutputSafetyState::TorqueUnknown; // 与 core 类型保持链接一致性检查
    let report =
        assert_no_alloc::assert_no_alloc(|| black_box(run_hot_paths(black_box(&mut fixture))));
    assert!(report.first_publish_ok, "{report:?}");
    assert!(report.steady_publishes_ok, "{report:?}");
    assert!(report.steady_reads_consistent, "{report:?}");
    assert!(report.exhausted_reported, "{report:?}");
    assert!(report.exhausted_keeps_last, "{report:?}");
    assert!(report.codec_roundtrip_ok, "{report:?}");
    assert!(report.state_first_publish_ok, "{report:?}");
    assert!(report.state_steady_ok, "{report:?}");
    assert!(report.state_exhausted_reported, "{report:?}");
    assert!(report.state_codec_roundtrip_ok, "{report:?}");
    assert_eq!(report.version_rt, 2 * 33, "{report:?}");
    assert_eq!(report.version_state, 2 * 17, "{report:?}");
}
