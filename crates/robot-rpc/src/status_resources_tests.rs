//! 固定proc文本与可控时钟验证采样计算；HTTP测试不依赖真实/proc或硬件。

use super::*;
use axum::body::{Body, to_bytes};
use axum::http::Request;
use robot_core::time::{MockClock, MonoRawNs};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use tower::ServiceExt;

const STAT: &str = "cpu 100 20 30 400 10 5 6 9 999999 999999\ncpu0 50 10 15 200 5 3 3 4 0 0\ncpu1 50 10 15 200 5 2 3 5 0 0\nctxt 9007199254740993\n";
const MEMORY: &str =
    "MemTotal: 1024 kB\nMemAvailable: 400 kB\nSwapTotal: 128 kB\nSwapFree: 32 kB\n";
const LOAD: &str = "1.25 0.50 0.75 2/160 999\n";
const NETWORK: &str = "Inter-| Receive | Transmit\n face |bytes packets errs drop fifo frame compressed multicast|bytes packets errs drop fifo colls carrier compressed\n lo: 88 1 0 0 0 0 0 0 88 1 0 0 0 0 0 0\n eth0: 9007199254740993 1 0 0 0 0 0 0 2000 1 0 0 0 0 0 0\n wg0: 400 1 0 0 0 0 0 0 500 1 0 0 0 0 0 0\n";
const PROCESS: &str =
    "Name:\trobot-status\nVmRSS:\t64 kB\nThreads:\t5\nvoluntary_ctxt_switches:\t999\n";

fn sample() -> RawSample {
    parse(STAT, MEMORY, LOAD, NETWORK, PROCESS).unwrap()
}

#[test]
fn proc_fields_preserve_units_online_cores_tasks_and_exact_counters() {
    let sample = sample();
    let view = sample.view(1000, None);
    let json = serde_json::to_value(view).unwrap();
    assert_eq!(json["status"], "ok");
    assert_eq!(json["cpu"]["logical_cores"], 2);
    assert_eq!(
        json["cpu"]["load_average"],
        serde_json::json!([1.25, 0.5, 0.75])
    );
    assert_eq!(json["cpu"]["usage_percent"], serde_json::Value::Null);
    assert_eq!(json["memory"]["total_bytes"], 1024 * 1024);
    assert_eq!(json["memory"]["available_bytes"], 400 * 1024);
    assert_eq!(json["memory"]["used_bytes"], 624 * 1024);
    assert_eq!(json["memory"]["swap_total_bytes"], 128 * 1024);
    assert_eq!(json["memory"]["swap_used_bytes"], 96 * 1024);
    assert_eq!(json["tasks"], serde_json::json!({"total":160,"running":2}));
    assert_eq!(json["context_switches"]["total"], "9007199254740993");
    assert_eq!(
        json["context_switches"]["per_second"],
        serde_json::Value::Null
    );
    assert_eq!(json["networks"].as_array().unwrap().len(), 2);
    assert_eq!(json["networks"][0]["name"], "eth0");
    assert_eq!(json["networks"][0]["rx_bytes"], "9007199254740993");
    assert_eq!(
        json["networks"][0]["rx_bytes_per_second"],
        serde_json::Value::Null
    );
    assert_eq!(
        json["web_process"],
        serde_json::json!({"threads":5,"resident_bytes":65536})
    );
    assert_eq!(json["stamp_ns"], "1000");
    assert_eq!(json["interval_ms"], 2000);
    assert_eq!(json["stale_after_ms"], 6000);
}

#[test]
fn rates_use_actual_source_interval_and_idle_includes_iowait() {
    let previous = sample();
    let mut current = previous.clone();
    // 总增量100，idle+iowait增量70；guest的巨大值不参与计算。
    current.cpu.ticks[0] += 30;
    current.cpu.ticks[3] += 50;
    current.cpu.ticks[4] += 20;
    current.context_switches += 12;
    current.networks.get_mut("eth0").unwrap().rx += 300;
    current.networks.get_mut("eth0").unwrap().tx += 900;
    let view = current.view(3_000_000_001, Some(&(1, previous)));
    assert_eq!(view.cpu.unwrap().usage_percent, Some(30.0));
    assert_eq!(view.context_switches.unwrap().per_second, Some(4.0));
    assert_eq!(view.networks[0].rx_bytes_per_second, Some(100.0));
    assert_eq!(view.networks[0].tx_bytes_per_second, Some(300.0));
    assert_eq!(view.networks[1].rx_bytes_per_second, Some(0.0));
    assert_eq!(view.networks[1].tx_bytes_per_second, Some(0.0));
}

#[test]
fn unchanged_valid_counters_are_zero_rates_but_unchanged_cpu_is_unknown() {
    let raw = sample();
    let view = raw.view(2_000_000_001, Some(&(1, raw.clone())));
    assert_eq!(view.cpu.unwrap().usage_percent, None);
    assert_eq!(view.context_switches.unwrap().per_second, Some(0.0));
    assert!(
        view.networks
            .iter()
            .all(|network| network.rx_bytes_per_second == Some(0.0)
                && network.tx_bytes_per_second == Some(0.0))
    );
}

#[test]
fn cpu_component_rollback_core_changes_and_overflow_do_not_fabricate_usage() {
    let previous = sample();
    let mut current = previous.clone();
    current.cpu.ticks[0] += 100;
    current.cpu.ticks[4] -= 1;
    assert_eq!(cpu_usage(&current.cpu, &previous.cpu), None);
    current.cpu = previous.cpu.clone();
    current.cpu.logical_cores = 1;
    current.cpu.ticks[0] += 100;
    assert_eq!(cpu_usage(&current.cpu, &previous.cpu), None);
    let overflow = CpuCounters {
        ticks: [u64::MAX; 8],
        logical_cores: 2,
    };
    let zero = CpuCounters {
        ticks: [0; 8],
        logical_cores: 2,
    };
    assert_eq!(cpu_usage(&overflow, &zero), None);
    let mut idle = previous.cpu.clone();
    idle.ticks[3] += 100;
    assert_eq!(cpu_usage(&idle, &previous.cpu), Some(0.0));
}

#[test]
fn counter_and_clock_rollback_and_new_interfaces_require_a_new_baseline() {
    let previous = sample();
    let mut current = previous.clone();
    current.context_switches -= 1;
    let eth = current.networks.get_mut("eth0").unwrap();
    eth.rx -= 1;
    eth.tx += 200;
    current.networks.remove("wg0");
    current
        .networks
        .insert("usb0".into(), NetworkCounters { rx: 100, tx: 200 });
    let view = current.view(2_000_000_001, Some(&(1, previous.clone())));
    assert_eq!(view.context_switches.unwrap().per_second, None);
    assert_eq!(view.networks.len(), 2);
    assert_eq!(view.networks[0].rx_bytes_per_second, None);
    assert_eq!(view.networks[0].tx_bytes_per_second, Some(100.0));
    assert_eq!(view.networks[1].name, "usb0");
    assert_eq!(view.networks[1].rx_bytes_per_second, None);
    // 上一份完整快照不含消失的接口；再次出现不能跨缺失窗口续算。
    let reappeared = previous.view(4_000_000_001, Some(&(2_000_000_001, current)));
    assert_eq!(reappeared.networks[1].name, "wg0");
    assert_eq!(reappeared.networks[1].rx_bytes_per_second, None);
    for now in [1, 0] {
        let view = previous.view(now, Some(&(1, previous.clone())));
        assert_eq!(view.cpu.unwrap().usage_percent, None);
        assert_eq!(view.context_switches.unwrap().per_second, None);
        assert!(
            view.networks
                .iter()
                .all(|network| network.rx_bytes_per_second.is_none())
        );
    }
}

#[test]
fn missing_malformed_duplicate_and_nonfinite_proc_fields_are_rejected() {
    for stat in [
        STAT.replace("cpu 100 20 30 400 10 5 6 9", "cpu 100 20"),
        STAT.replace("cpu0 50", "cpu0 nope"),
        STAT.replace("ctxt 9007199254740993", "ctxt -1"),
        STAT.replace("ctxt 9007199254740993", ""),
        format!("{STAT}ctxt 1\n"),
        STAT.replace("cpu1 ", "cpu0 "),
    ] {
        assert!(parse(&stat, MEMORY, LOAD, NETWORK, PROCESS).is_err());
    }
    for memory in [
        MEMORY.replace("MemAvailable: 400 kB", ""),
        MEMORY.replace("MemAvailable: 400", "MemAvailable: 2048"),
        MEMORY.replace("SwapFree: 32", "SwapFree: 256"),
        MEMORY.replace("MemTotal: 1024", "MemTotal: 18446744073709551615"),
        MEMORY.replace("MemTotal: 1024 kB", "MemTotal: 1024 MB"),
        format!("{MEMORY}MemTotal: 1 kB\n"),
    ] {
        assert!(parse(STAT, &memory, LOAD, NETWORK, PROCESS).is_err());
    }
    for load in [
        "NaN 0 0 1/2 3",
        "inf 0 0 1/2 3",
        "-1 0 0 1/2 3",
        "0 0 0 3/2 3",
        "0 0 0 1/2",
        "0 0 0 1/nope 3",
    ] {
        assert!(parse(STAT, MEMORY, load, NETWORK, PROCESS).is_err());
    }
    for network in [
        NETWORK.replace("2000 1", "nope 1"),
        NETWORK.replace("500 1 0 0 0 0 0 0", "500"),
        "broken\n".into(),
    ] {
        assert!(parse(STAT, MEMORY, LOAD, &network, PROCESS).is_err());
    }
    for process in [
        PROCESS.replace("Threads:\t5", "Threads: 0"),
        PROCESS.replace("VmRSS:\t64 kB", "VmRSS: 64 bytes"),
        format!("{PROCESS}Threads: 2\n"),
    ] {
        assert!(parse(STAT, MEMORY, LOAD, NETWORK, &process).is_err());
    }
}

#[test]
fn only_loopback_is_a_valid_empty_network_list() {
    let network = NETWORK.lines().take(3).collect::<Vec<_>>().join("\n");
    assert!(
        parse(STAT, MEMORY, LOAD, &network, PROCESS)
            .unwrap()
            .networks
            .is_empty()
    );
}

#[test]
fn proc_reads_are_bounded_and_missing_files_report_unavailable() {
    let root = tempfile::tempdir().unwrap();
    assert!(read_proc(root.path()).err().unwrap().contains("stat"));
    std::fs::write(
        root.path().join("stat"),
        vec![b'x'; usize::try_from(MAX_PROC_BYTES + 1).unwrap()],
    )
    .unwrap();
    assert!(read_proc(root.path()).err().unwrap().contains("256KiB"));
}

#[test]
fn stale_and_future_cache_ages_never_report_live_values() {
    let view = sample().view(1, None);
    assert_eq!(view.at_time(6_000_000_001).status, "ok");
    let stale = view.at_time(6_000_000_002);
    assert_eq!(stale.status, "unavailable");
    assert_eq!(stale.stamp_ns.as_deref(), Some("1"));
    assert_eq!(stale.age_ms, Some(6000));
    assert!(stale.cpu.is_none() && stale.memory.is_none() && stale.networks.is_empty());
    let future = view.at_time(0);
    assert_eq!(future.status, "unavailable");
    assert!(future.cpu.is_none());
}

#[tokio::test]
async fn concurrent_gets_share_cache_and_cannot_launch_or_trigger_sampling() {
    let clock = Arc::new(MockClock::new(MonoRawNs(1)));
    let state = ResourcesState::new(clock);
    *state.latest.lock().unwrap() = sample().view(1, None);
    let reads = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&reads);
    let (entered, read_started) = mpsc::sync_channel(1);
    let (release, released) = mpsc::sync_channel(1);
    let (stop, stopped) = watch::channel(false);
    let worker = state
        .start_with_reader(stopped, move || {
            observed.fetch_add(1, Ordering::Relaxed);
            entered.send(()).map_err(|_| "测试通知已关闭")?;
            released.recv().map_err(|_| "测试采样已取消")?;
            Ok(sample())
        })
        .unwrap();
    read_started.recv_timeout(Duration::from_secs(2)).unwrap();
    let app = router(state.clone());
    let mut requests = tokio::task::JoinSet::new();
    for _ in 0..64 {
        let app = app.clone();
        requests.spawn(async move {
            let response = app
                .oneshot(
                    Request::builder()
                        .uri("/api/resources")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            serde_json::from_slice::<serde_json::Value>(
                &to_bytes(response.into_body(), 32_768).await.unwrap(),
            )
            .unwrap()
        });
    }
    while let Some(response) = requests.join_next().await {
        let json = response.unwrap();
        assert_eq!(json["status"], "ok");
        assert_eq!(json["stamp_ns"], "1");
        assert_eq!(json["age_ms"], 0);
    }
    assert_eq!(reads.load(Ordering::Relaxed), 1);
    stop.send(true).unwrap();
    release.send(()).unwrap();
    worker.thread().unpark();
    worker.join().unwrap().unwrap();
    assert_eq!(reads.load(Ordering::Relaxed), 1);
    assert_eq!(state.snapshot().status, "ok");
}

#[test]
fn pre_stopped_worker_is_reclaimed_without_any_read() {
    let state = ResourcesState::new(Arc::new(MockClock::new(MonoRawNs(1))));
    let (_stop, stopped) = watch::channel(true);
    let worker = state
        .start_with_reader(stopped, || panic!("停止后不得采样"))
        .unwrap();
    worker.join().unwrap().unwrap();
    let view = state.snapshot();
    assert_eq!(view.status, "unavailable");
    assert_eq!(view.stamp_ns, None);
    assert!(view.cpu.is_none());
}

#[cfg(not(target_os = "linux"))]
#[test]
fn unsupported_platform_is_explicitly_unavailable() {
    assert!(read_system().err().unwrap().contains("仅支持Linux"));
}
