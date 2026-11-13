//! 固定cgroup/proc目录验证计费、辅助进程、差分身份和错误隔离。

use super::*;
use std::fmt::Write as _;

struct Fixture {
    temporary: tempfile::TempDir,
    root: PathBuf,
    proc: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("system.slice");
        let proc = temporary.path().join("proc");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&proc).unwrap();
        Self {
            temporary,
            root,
            proc,
        }
    }

    fn service(&self, name: &str, pids: &[u32], threads: u64) -> PathBuf {
        let path = self.root.join(name);
        std::fs::create_dir_all(&path).unwrap();
        Self::procs(&path, pids);
        std::fs::write(
            path.join("cpu.stat"),
            "usage_usec 1000000\nuser_usec 700000\nsystem_usec 300000\n",
        )
        .unwrap();
        std::fs::write(path.join("memory.current"), "4096\n").unwrap();
        std::fs::write(
            path.join("memory.stat"),
            "anon 1024\nfile 3072\nshmem 2048\n",
        )
        .unwrap();
        std::fs::write(path.join("pids.current"), threads.to_string()).unwrap();
        path
    }

    fn procs(path: &Path, pids: &[u32]) {
        std::fs::create_dir_all(path).unwrap();
        let mut body = String::new();
        for pid in pids {
            writeln!(body, "{pid}").unwrap();
        }
        std::fs::write(path.join("cgroup.procs"), body).unwrap();
    }

    fn thread(&self, pid: u32, tid: u32, start: u64, voluntary: u64, nonvoluntary: u64) {
        let path = self
            .proc
            .join(pid.to_string())
            .join("task")
            .join(tid.to_string());
        std::fs::create_dir_all(&path).unwrap();
        let mut fields = vec!["0".to_owned(); 20];
        fields[0] = "S".into();
        fields[19] = start.to_string();
        std::fs::write(
            path.join("stat"),
            format!("{tid} (worker ) 名称) {}\n", fields.join(" ")),
        )
        .unwrap();
        std::fs::write(path.join("status"), format!(
            "Name:\tworker\nTgid:\t{pid}\nPid:\t{tid}\nvoluntary_ctxt_switches:\t{voluntary}\nnonvoluntary_ctxt_switches:\t{nonvoluntary}\n"
        )).unwrap();
    }

    fn read(&self) -> RawSnapshot {
        read(&self.root, &self.proc)
    }
}

#[test]
fn recursive_helpers_and_threads_are_counted_without_adding_rss_or_child_memory() {
    let fixture = Fixture::new();
    let path = fixture.service("robot-s0-bus1.service", &[10], 3);
    let child = path.join("camera-helper");
    Fixture::procs(&child, &[10, 20]);
    // 顶层memory.current已经包含子组；子组数值不能再次相加。
    std::fs::write(child.join("memory.current"), "999999\n").unwrap();
    fixture.thread(10, 10, 100, 20, 30);
    fixture.thread(10, 11, 101, 40, 50);
    fixture.thread(20, 20, 200, 60, 70);
    let view = fixture.read().view(1, None, 6);
    assert_eq!(view.status, "ok");
    assert_eq!(view.service_count, 1);
    assert_eq!(view.totals.memory_bytes, Some(4096));
    assert_eq!(view.totals.shmem_bytes, Some(2048));
    assert_eq!(view.totals.processes, Some(2));
    assert_eq!(view.totals.threads, Some(3));
    assert_eq!(view.totals.cpu_cores, None);
    assert_eq!(view.totals.context_switches_per_second, None);
    assert_eq!(view.services[0].status, "running");
    assert!(view.services[0].description.contains("辅助进程"));
}

#[test]
fn rates_include_all_threads_and_cpu_percent_uses_machine_core_count() {
    let fixture = Fixture::new();
    let path = fixture.service("robot-s0-bus1.service", &[10, 20], 3);
    fixture.thread(10, 10, 100, 20, 30);
    fixture.thread(10, 11, 101, 40, 50);
    fixture.thread(20, 20, 200, 60, 70);
    let old = fixture.read();
    std::fs::write(path.join("cpu.stat"), "usage_usec 3000000\n").unwrap();
    fixture.thread(10, 10, 100, 30, 30);
    fixture.thread(10, 11, 101, 40, 60);
    fixture.thread(20, 20, 200, 60, 80);
    let view = fixture.read().view(2_000_000_001, Some((1, &old)), 6);
    assert_eq!(view.status, "ok");
    assert_eq!(view.totals.cpu_cores, Some(1.0));
    assert!((view.totals.cpu_percent.unwrap() - 100.0 / 6.0).abs() < 1e-9);
    assert_eq!(view.totals.context_switches_per_second, Some(15.0));
    let zero = fixture
        .read()
        .view(4_000_000_001, Some((2_000_000_001, &fixture.read())), 6);
    assert_eq!(zero.totals.cpu_cores, Some(0.0));
    assert_eq!(zero.totals.context_switches_per_second, Some(0.0));
}

#[test]
fn shared_memory_is_a_separate_submetric_and_cross_file_time_skew_is_preserved() {
    let fixture = Fixture::new();
    let first = fixture.service("robot-status.service", &[], 0);
    let second = fixture.service("robot-s1-bus1.service", &[], 0);
    std::fs::write(first.join("memory.current"), "100\n").unwrap();
    std::fs::write(first.join("memory.stat"), "shmem 120\n").unwrap();
    std::fs::write(second.join("memory.current"), "200\n").unwrap();
    std::fs::write(second.join("memory.stat"), "shmem 50\n").unwrap();
    let view = fixture.read().view(1, None, 6);
    assert_eq!(view.status, "ok");
    assert_eq!(view.totals.memory_bytes, Some(300));
    assert_eq!(view.totals.shmem_bytes, Some(170));
    assert_eq!(view.services[1].status, "inactive");
    assert!(
        view.services
            .iter()
            .any(|service| service.description.contains("动作后端"))
    );
}

#[test]
fn cgroup_recreation_and_cpu_rollback_reset_cpu_baseline() {
    let fixture = Fixture::new();
    let path = fixture.service("robot-status.service", &[10], 1);
    fixture.thread(10, 10, 100, 20, 30);
    let old = fixture.read();
    std::fs::rename(&path, fixture.temporary.path().join("old-service")).unwrap();
    fixture.service("robot-status.service", &[10], 1);
    let recreated = fixture.read();
    assert_eq!(
        recreated
            .view(2_000_000_001, Some((1, &old)), 6)
            .totals
            .cpu_cores,
        None
    );
    assert_eq!(
        recreated
            .view(2_000_000_001, Some((1, &old)), 6)
            .totals
            .context_switches_per_second,
        None
    );
    std::fs::write(path.join("cpu.stat"), "usage_usec 1\n").unwrap();
    assert_eq!(
        fixture
            .read()
            .view(4_000_000_001, Some((2_000_000_001, &recreated)), 6)
            .totals
            .cpu_cores,
        None
    );
}

#[test]
fn tid_reuse_member_changes_and_counter_rollback_do_not_fake_context_rates() {
    let fixture = Fixture::new();
    let path = fixture.service("robot-status.service", &[10], 1);
    fixture.thread(10, 10, 100, 20, 30);
    let old = fixture.read();
    fixture.thread(10, 10, 999, 200, 300);
    let reused = fixture.read();
    assert_eq!(
        reused
            .view(2_000_000_001, Some((1, &old)), 6)
            .totals
            .context_switches_per_second,
        None
    );
    fixture.thread(10, 10, 999, 1, 1);
    assert_eq!(
        fixture
            .read()
            .view(4_000_000_001, Some((2_000_000_001, &reused)), 6)
            .totals
            .context_switches_per_second,
        None
    );
    fixture.thread(10, 11, 1000, 10, 20);
    std::fs::write(path.join("pids.current"), "2\n").unwrap();
    assert_eq!(
        fixture
            .read()
            .view(6_000_000_001, Some((1, &old)), 6)
            .totals
            .context_switches_per_second,
        None
    );
    Fixture::procs(&path, &[10, 20]);
    fixture.thread(20, 20, 2000, 0, 0);
    std::fs::write(path.join("pids.current"), "3\n").unwrap();
    assert_eq!(
        fixture
            .read()
            .view(8_000_000_001, Some((1, &old)), 6)
            .totals
            .context_switches_per_second,
        None
    );
}

#[test]
fn thread_read_failure_is_partial_and_keeps_other_metrics() {
    let fixture = Fixture::new();
    fixture.service("robot-status.service", &[10], 1);
    fixture.thread(10, 10, 100, 20, 30);
    std::fs::remove_file(fixture.proc.join("10/task/10/status")).unwrap();
    let view = fixture.read().view(1, None, 6);
    assert_eq!(view.status, "partial");
    assert_eq!(view.services[0].status, "running");
    assert!(view.services[0].error.is_some());
    assert_eq!(view.totals.memory_bytes, Some(4096));
    assert_eq!(view.totals.processes, Some(1));
    assert_eq!(view.totals.threads, Some(1));
    assert_eq!(view.totals.context_switches_per_second, None);
}

#[test]
fn permission_error_never_exposes_partial_sum_as_the_robot_total() {
    let fixture = Fixture::new();
    fixture.service("robot-status.service", &[], 0);
    let mut raw = fixture.read();
    raw.entries.as_mut().unwrap().insert(
        "robot-s0-bus1.service".into(),
        Err("Permission denied".into()),
    );
    let view = raw.view(1, None, 6);
    assert_eq!(view.status, "partial");
    assert_eq!(view.service_count, 2);
    assert!(view.totals.memory_bytes.is_none());
    assert!(view.totals.shmem_bytes.is_none());
    assert!(view.totals.processes.is_none());
    assert!(view.totals.threads.is_none());
    assert_eq!(view.services[0].status, "unavailable");
}

#[test]
fn service_removal_and_empty_scope_are_complete_snapshots() {
    let fixture = Fixture::new();
    let removed = fixture.service("robot-s1-bus1.service", &[], 0);
    fixture.service("robot-status.service", &[], 0);
    let old = fixture.read();
    std::fs::remove_dir_all(removed).unwrap();
    let view = fixture.read().view(2_000_000_001, Some((1, &old)), 6);
    assert_eq!(view.status, "ok");
    assert_eq!(view.service_count, 1);
    assert_eq!(view.services[0].name, "robot-status.service");
    assert_eq!(view.totals.cpu_cores, Some(0.0));
    std::fs::remove_dir_all(fixture.root.join("robot-status.service")).unwrap();
    let empty = fixture.read().view(1, None, 6);
    assert_eq!(empty.status, "ok");
    assert_eq!(empty.service_count, 0);
    assert_eq!(empty.totals.cpu_cores, Some(0.0));
    assert_eq!(empty.totals.memory_bytes, Some(0));
    assert_eq!(empty.totals.context_switches_per_second, Some(0.0));
}

#[test]
fn missing_scope_and_exceeded_limits_are_errors_without_truncating_totals() {
    let fixture = Fixture::new();
    let missing = read(&fixture.root.join("missing"), &fixture.proc).view(1, None, 6);
    assert_eq!(missing.status, "unavailable");
    assert_eq!(missing.totals.memory_bytes, None);
    for index in 0..=MAX_SERVICES {
        fixture.service(&format!("robot-{index}.service"), &[], 0);
    }
    let over = fixture.read().view(1, None, 6);
    assert_eq!(over.status, "unavailable");
    assert_eq!(over.service_count, 0);
    assert_eq!(over.totals.memory_bytes, None);
    let mut budget = Budget {
        files: MAX_FILES,
        ..Budget::default()
    };
    assert!(
        budget
            .read(&fixture.root.join("missing"))
            .unwrap_err()
            .contains("8192")
    );
}

#[test]
fn malformed_ids_stat_status_and_duplicate_fields_are_rejected() {
    assert!(start_time("10 (unfinished S 0", 10).is_err());
    assert!(start_time("11 (name) S 0", 10).is_err());
    assert!(key("shmem 1\nshmem 2\n", "shmem").is_err());
    let fixture = Fixture::new();
    let path = fixture.service("robot-status.service", &[10], 1);
    fixture.thread(10, 10, 100, 20, 30);
    std::fs::write(
        fixture.proc.join("10/task/10/status"),
        "Pid: 10\nTgid: 99\nvoluntary_ctxt_switches: 1\nnonvoluntary_ctxt_switches: 1\n",
    )
    .unwrap();
    assert_eq!(fixture.read().view(1, None, 6).status, "partial");
    std::fs::write(path.join("cgroup.procs"), "0\n").unwrap();
    let view = fixture.read().view(1, None, 6);
    assert_eq!(view.services[0].status, "unavailable");
    assert_eq!(view.totals.processes, None);
}

#[test]
fn duplicate_pid_across_services_is_not_counted_twice() {
    let fixture = Fixture::new();
    fixture.service("robot-status.service", &[10], 1);
    fixture.service("robot-other.service", &[10], 1);
    fixture.thread(10, 10, 100, 0, 0);
    let view = fixture.read().view(1, None, 6);
    assert_eq!(view.status, "partial");
    assert!(
        view.services
            .iter()
            .all(|service| service.status == "unavailable")
    );
    assert_eq!(view.totals.processes, None);
}

#[test]
fn zero_or_backward_time_and_unknown_core_count_do_not_create_cpu_percentage() {
    let fixture = Fixture::new();
    fixture.service("robot-status.service", &[], 0);
    let old = fixture.read();
    for stamp in [0, 1] {
        let view = fixture.read().view(stamp, Some((1, &old)), 6);
        assert_eq!(view.totals.cpu_cores, None);
        assert_eq!(view.totals.context_switches_per_second, None);
    }
    let view = fixture.read().view(2_000_000_001, Some((1, &old)), 0);
    assert_eq!(view.totals.cpu_cores, Some(0.0));
    assert_eq!(view.totals.cpu_percent, None);
}
