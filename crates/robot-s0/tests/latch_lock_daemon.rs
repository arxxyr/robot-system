//! S48 A3d / W1：真实 `robot-s0` 进程级的 latch 稳定锁与启动恢复门。
//!
//! 每个用例都用 `CARGO_BIN_EXE_robot-s0` 拉起一个独立子进程（独立临时目录、独立
//! `robot_id`、独立信任目录），只从退出码、日志文案与退出后的真实文件态取证；不依赖
//! `test-comm-fault-hook` 特性，也不读观测页。用例顺序对应 daemon 的启动链：名字校验 →
//! 信任锁 → 维护子命令 → latch 稳定锁 → 数据对象检查 → W0 恢复 → adapter 构造。所有失败
//! 用例都断言日志不含 `initial estop_latched`，证明失败发生在 adapter 构造之前。
//!
//! 运行：`cargo test -p robot-s0 --test latch_lock_daemon`。

#![cfg(target_os = "linux")]

use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use robot_core::time::{RobotClock, SystemClock};
use robot_s0::clear_estop_transaction::{ClearTxnStore, NoGuard};
use robot_s0::latch_lock::acquire_latch_lock;

/// 并行用例的实例计数；与本进程 pid 一起拼进 `robot_id`，避免 iceoryx2 服务名冲突。
static NEXT_INSTANCE: AtomicU32 = AtomicU32::new(0);

/// latch primary 文件名；派生文件 `.bak` / `.clear_pending` / `.lock` 与之同目录。
const LATCH_NAME: &str = "estop_latch";
/// 正常路径：READY 后 smoke 到期自行退出，含 iceoryx2 建服务与线程收尾。
const NORMAL_EXIT_TIMEOUT_MS: u64 = 40_000;
/// 失败路径：启动链早退，不建 adapter、不进 iceoryx2。
const FAILURE_EXIT_TIMEOUT_MS: u64 = 15_000;
/// adapter 构造之前才会打印的初始 latch 值；失败用例断言它不存在。
const INITIAL_LATCHED: &str = "initial estop_latched";
const INITIAL_LATCHED_TRUE: &str = "initial estop_latched latched=true";
const INITIAL_LATCHED_FALSE: &str = "initial estop_latched latched=false";

/// 一个隔离的 daemon 实例目录：配置 TOML、latch 目标、信任目录与日志文件都在其中。
struct Fixture {
    directory: tempfile::TempDir,
    robot_id: String,
}

impl Fixture {
    /// 建临时目录、写 `mock_serial_rt` 三关节配置（`auto_enable=false`）并预建信任目录；
    /// latch 双份由各用例按需写入。
    fn create() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let pid = std::process::id();
        let instance = NEXT_INSTANCE.fetch_add(1, Ordering::Relaxed);
        let robot_id = format!("latch_gate_{pid}_{instance}");
        let config = format!(
            "[robot]\nid={robot_id:?}\nconfig_version=\"0.1.2\"\n[[hardware]]\nid=\"arm\"\n\
             adapter=\"mock_serial_rt\"\njoints=[\"j0\",\"j1\",\"j2\"]\nmotor_ids=[1,2,3]\n\
             auto_enable=false\n"
        );
        std::fs::write(directory.path().join("config.toml"), config).unwrap();
        std::fs::create_dir(directory.path().join("trust")).unwrap();
        Self {
            directory,
            robot_id,
        }
    }

    fn dir(&self) -> &Path {
        self.directory.path()
    }

    /// `<dir>/estop_latch`。
    fn latch(&self) -> PathBuf {
        self.dir().join(LATCH_NAME)
    }

    /// `<dir>/estop_latch.bak`。
    fn backup(&self) -> PathBuf {
        self.dir().join(format!("{LATCH_NAME}.bak"))
    }

    /// `<dir>/estop_latch.clear_pending`（W0 事务标记）。
    fn marker(&self) -> PathBuf {
        self.dir().join(format!("{LATCH_NAME}.clear_pending"))
    }

    /// `<dir>/estop_latch.lock`（稳定锁，永不删除，内容为持锁者 pid）。
    fn lock(&self) -> PathBuf {
        self.dir().join(format!("{LATCH_NAME}.lock"))
    }

    /// `<dir>/trust`（`rx_trust.lock` 与信任标记所在目录）。
    fn trust(&self) -> PathBuf {
        self.dir().join("trust")
    }

    fn config(&self) -> PathBuf {
        self.dir().join("config.toml")
    }

    fn log_path(&self) -> PathBuf {
        self.dir().join("s0.log")
    }

    /// 把 primary 与 backup 都写成同一个值（`0\n` 或 `1\n`）。
    fn write_pair(&self, value: &[u8]) {
        std::fs::write(self.latch(), value).unwrap();
        std::fs::write(self.backup(), value).unwrap();
    }

    /// 以隔离环境拉起 `robot-s0`：清掉 systemd 相关与全部 `ROBOT_TEST_*` 变量，固定
    /// 人类可读日志格式且关闭 ANSI（字段 `key=value` 才能按子串断言），stdout / stderr
    /// 一并重定向到临时目录的 `s0.log`。
    fn spawn(&self, configure: impl FnOnce(&mut Command)) -> Daemon {
        let log = std::fs::File::create(self.log_path()).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_robot-s0"));
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("ROBOT_TEST_") {
                command.env_remove(key);
            }
        }
        command
            .env_remove("NOTIFY_SOCKET")
            .env_remove("INVOCATION_ID")
            .env_remove("WATCHDOG_USEC")
            .env_remove("ROBOT_LOG_FORMAT")
            .env("NO_COLOR", "1")
            .env("RUST_LOG", "info")
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log);
        configure(&mut command);
        let child = command.spawn().unwrap();
        Daemon {
            child,
            log_path: self.log_path(),
        }
    }

    /// 正常 daemon 参数：READY 后约 300 ms 自行退出，退出时把 latch 双份重写为当前值。
    /// `latch` 单独传入，便于用例给出非法名字。
    fn spawn_daemon(&self, latch: &Path) -> Daemon {
        self.spawn(|command| {
            command
                .arg("--config")
                .arg(self.config())
                .args(["--hardware-id", "arm", "--latch-path"])
                .arg(latch)
                .arg("--rx-trust-dir")
                .arg(self.trust())
                .args(["--smoke-duration", "300ms"])
                .args(["--notify-mode", "standalone"]);
        })
    }

    /// 维护子命令 `rx-trust status`：不能带 `--config` / `--hardware-id`。
    fn spawn_rx_trust_status(&self) -> Daemon {
        self.spawn(|command| {
            command
                .args(["rx-trust", "status", "--robot-id"])
                .arg(&self.robot_id)
                .arg("--latch-path")
                .arg(self.latch())
                .arg("--rx-trust-dir")
                .arg(self.trust());
        })
    }
}

/// 一个已拉起的 `robot-s0` 子进程；drop 时若仍存活则 kill，防止残留进程占住服务名。
struct Daemon {
    child: Child,
    log_path: PathBuf,
}

impl Daemon {
    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log_path).unwrap()
    }

    /// 轮询 `try_wait` 直到退出并返回退出码；被信号终止或超时都 panic 并附完整日志，
    /// 超时时先 kill 子进程。
    fn wait_exit(&mut self, timeout_ms: u64) -> i32 {
        let deadline = SystemClock.now().0 + timeout_ms * 1_000_000;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status
                    .code()
                    .unwrap_or_else(|| panic!("子进程意外被信号终止：{status}\n{}", self.log()));
            }
            if SystemClock.now().0 >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                panic!("等待退出超时（{timeout_ms} ms）\n{}", self.log());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn read(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_else(|error| panic!("读取 {} 失败: {error}", path.display()))
}

fn pid_line(pid: u32) -> Vec<u8> {
    format!("{pid}\n").into_bytes()
}

/// 只把真实 `NotFound` 记为缺失；权限等其它错误不得伪装成"不存在"。
fn is_absent(path: &Path) -> bool {
    matches!(
        std::fs::symlink_metadata(path),
        Err(error) if error.kind() == ErrorKind::NotFound
    )
}

/// 失败用例共同判据：日志里没有 adapter 构造之前才会打印的 `initial estop_latched`。
fn assert_failed_before_adapter(log: &str) {
    assert!(
        !log.contains(INITIAL_LATCHED),
        "失败必须先于 adapter 构造:\n{log}"
    );
}

/// 用 W0 公开 API 制造一个 pending 标记：双份先置 `1`，`prepare_clear` 之后盘上双份为
/// `0` 且标记存在；凭证 drop 不删除标记，标记身份由 `robot_id` 决定。
fn prepare_pending(fixture: &Fixture, robot_id: &str) {
    fixture.write_pair(b"1\n");
    let mut store = ClearTxnStore::open(&fixture.latch(), robot_id, "arm").unwrap();
    let _prepared = store.prepare_clear(7, 1, true, &mut NoGuard).unwrap();
    assert!(fixture.marker().exists());
    assert_eq!(read(&fixture.latch()), b"0\n");
    assert_eq!(read(&fixture.backup()), b"0\n");
}

/// 无标记、双份一致的有效启动：恢复完成、按值打印初始 latch、退出 0；退出后双份被重写为
/// 同一个值，锁文件留着且内容是 daemon 自己的 pid。
fn run_valid_pair(value: &[u8], expected_initial: &str) {
    let fixture = Fixture::create();
    fixture.write_pair(value);

    let mut daemon = fixture.spawn_daemon(&fixture.latch());
    let pid = daemon.pid();
    assert_eq!(daemon.wait_exit(NORMAL_EXIT_TIMEOUT_MS), 0);
    let log = daemon.log();
    assert!(log.contains("latch 启动恢复完成"), "{log}");
    assert!(log.contains(expected_initial), "{log}");
    assert_eq!(read(&fixture.latch()), value);
    assert_eq!(read(&fixture.backup()), value);
    assert_eq!(read(&fixture.lock()), pid_line(pid));
}

/// 1. 测试进程自己持有 latch 稳定锁：daemon 退 89（`LATCH_LOCK_HELD_EXIT_CODE`）、日志
///    `latch 锁被占用`；失败竞争者不改双份、不改锁文件里的持有者 pid。
#[test]
fn held_latch_lock_exits_89_and_keeps_holder_pid_and_pair() {
    let fixture = Fixture::create();
    fixture.write_pair(b"0\n");
    let _held = acquire_latch_lock(&fixture.latch()).unwrap();
    let own_pid = pid_line(std::process::id());
    assert_eq!(read(&fixture.lock()), own_pid);

    let mut daemon = fixture.spawn_daemon(&fixture.latch());
    assert_eq!(daemon.wait_exit(FAILURE_EXIT_TIMEOUT_MS), 89);
    let log = daemon.log();
    assert!(log.contains("latch 锁被占用"), "{log}");
    assert_failed_before_adapter(&log);
    assert_eq!(read(&fixture.latch()), b"0\n");
    assert_eq!(read(&fixture.backup()), b"0\n");
    assert_eq!(read(&fixture.lock()), own_pid);
}

/// 2. 非法 primary 名字（`estop_latch.bak`）：退 1、日志 `latch 名字不合法`；名字校验早于
///    任何锁写入：预置的 `rx_trust.lock` 内容原样保留，目录里没有任何 `.lock` 文件。
#[test]
fn invalid_latch_name_fails_before_any_lock_is_written() {
    let fixture = Fixture::create();
    fixture.write_pair(b"0\n");
    let trust_lock = fixture.trust().join("rx_trust.lock");
    std::fs::write(&trust_lock, b"424242\n").unwrap();
    let illegal = fixture.backup();

    let mut daemon = fixture.spawn_daemon(&illegal);
    assert_eq!(daemon.wait_exit(FAILURE_EXIT_TIMEOUT_MS), 1);
    let log = daemon.log();
    assert!(log.contains("latch 名字不合法"), "{log}");
    assert_failed_before_adapter(&log);
    assert_eq!(read(&trust_lock), b"424242\n");
    assert!(is_absent(&fixture.dir().join("estop_latch.bak.lock")));
    let locks: Vec<String> = std::fs::read_dir(fixture.dir())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| Path::new(name).extension().is_some_and(|ext| ext == "lock"))
        .collect();
    assert!(locks.is_empty(), "{locks:?}");
    assert_eq!(read(&fixture.latch()), b"0\n");
    assert_eq!(read(&illegal), b"0\n");
}

/// 3. 测试进程持 latch 锁时运行 `rx-trust status`：只取信任锁（`rx_trust.lock` 记录
///    daemon pid），精确退 0 且打印本夹具的真实标记状态 `primary=Missing backup=Missing`
///    与 `outcome=Complete`，日志不含 `latch 锁`，latch 锁文件里的持有者 pid 与双份不变；
///    维护子命令不取 latch 锁。
#[test]
fn rx_trust_status_runs_without_taking_latch_lock() {
    let fixture = Fixture::create();
    fixture.write_pair(b"0\n");
    let _held = acquire_latch_lock(&fixture.latch()).unwrap();
    let own_pid = pid_line(std::process::id());

    let mut daemon = fixture.spawn_rx_trust_status();
    let daemon_pid = daemon.pid();
    let code = daemon.wait_exit(FAILURE_EXIT_TIMEOUT_MS);
    let log = daemon.log();
    // `status` 固定 Complete → 0；CLI 解析错误也是 2，故必须精确断言 0，并以维护分流
    // 打印的真实状态行（本夹具无信任标记：双份 Missing、outcome=Complete）证明分流
    // 确实执行，而不是没走到分流就以别的码退出。
    assert_eq!(code, 0, "{log}");
    assert!(
        log.contains("primary=Missing backup=Missing"),
        "维护分流必须打印真实标记状态:\n{log}"
    );
    assert!(log.contains("outcome=Complete"), "{log}");
    assert!(!log.contains("latch 锁"), "{log}");
    assert_failed_before_adapter(&log);
    // 信任锁由维护子命令正常取得（记录 daemon pid）；latch 锁与双份完全不受影响。
    assert_eq!(
        read(&fixture.trust().join("rx_trust.lock")),
        pid_line(daemon_pid)
    );
    assert_eq!(read(&fixture.lock()), own_pid);
    assert_eq!(read(&fixture.latch()), b"0\n");
    assert_eq!(read(&fixture.backup()), b"0\n");
}

/// 4a. 无标记、双份 `1`：`latch 启动恢复完成` 与 `latched=true`、退出 0；退出后双份仍
///    `1`，`estop_latch.lock` 存在且内容为 daemon 的 pid。
#[test]
fn valid_latched_pair_starts_and_persists_true_on_exit() {
    run_valid_pair(b"1\n", INITIAL_LATCHED_TRUE);
}

/// 4b. 无标记、双份 `0`：同样走完整启动，初始值 `latched=false`，退出后双份仍 `0`。
#[test]
fn valid_clear_pair_starts_and_persists_false_on_exit() {
    run_valid_pair(b"0\n", INITIAL_LATCHED_FALSE);
}

/// 5. 严格缺失：primary 与 backup 都不存在 → 退 1，日志 `latch 启动恢复失败` 且阶段为
///    `LoadLatch`；退出后两份仍不存在（不自愈、不 provision）。
#[test]
fn missing_both_copies_fails_closed_without_provisioning() {
    let fixture = Fixture::create();

    let mut daemon = fixture.spawn_daemon(&fixture.latch());
    assert_eq!(daemon.wait_exit(FAILURE_EXIT_TIMEOUT_MS), 1);
    let log = daemon.log();
    assert!(log.contains("latch 启动恢复失败"), "{log}");
    assert!(log.contains("LoadLatch"), "{log}");
    assert_failed_before_adapter(&log);
    assert!(is_absent(&fixture.latch()), "primary 不得被 provision");
    assert!(is_absent(&fixture.backup()), "backup 不得被 provision");
    assert!(is_absent(&fixture.marker()));
}

/// 6. 有效 pending 标记（真实 `robot_id` + `arm` 制造）：启动恢复把双份修复为 `1` 并删除
///    标记，`latched=true`、退出 0；退出后双份仍 `1`、标记已删除。
#[test]
fn valid_pending_marker_recovers_to_latched_and_removes_marker() {
    let fixture = Fixture::create();
    prepare_pending(&fixture, &fixture.robot_id);
    assert!(fixture.marker().exists(), "凭证 drop 后标记必须仍在");

    let mut daemon = fixture.spawn_daemon(&fixture.latch());
    assert_eq!(daemon.wait_exit(NORMAL_EXIT_TIMEOUT_MS), 0);
    let log = daemon.log();
    assert!(log.contains("latch 启动恢复完成"), "{log}");
    assert!(log.contains(INITIAL_LATCHED_TRUE), "{log}");
    assert_eq!(read(&fixture.latch()), b"1\n");
    assert_eq!(read(&fixture.backup()), b"1\n");
    assert!(is_absent(&fixture.marker()), "恢复后标记必须删除");
}

/// 7. 损坏标记：`<latch>.clear_pending` 是 `garbage`（双份 `0`）→ 退 1、日志
///    `latch 启动恢复失败`；标记字节与双份都不变，坏标记不触发任何自愈。
#[test]
fn corrupted_marker_fails_recovery_and_keeps_all_bytes() {
    let fixture = Fixture::create();
    fixture.write_pair(b"0\n");
    std::fs::write(fixture.marker(), b"garbage").unwrap();

    let mut daemon = fixture.spawn_daemon(&fixture.latch());
    assert_eq!(daemon.wait_exit(FAILURE_EXIT_TIMEOUT_MS), 1);
    let log = daemon.log();
    assert!(log.contains("latch 启动恢复失败"), "{log}");
    assert_failed_before_adapter(&log);
    assert_eq!(read(&fixture.marker()), b"garbage");
    assert_eq!(read(&fixture.latch()), b"0\n");
    assert_eq!(read(&fixture.backup()), b"0\n");
}

/// 8. 身份错误标记：用 `other_robot` 身份制造的 pending 标记，以真实 `robot_id` 启动 →
///    退 1、日志 `latch 启动恢复失败`；标记与双份原样保留。
#[test]
fn foreign_identity_marker_fails_recovery_and_keeps_all_bytes() {
    let fixture = Fixture::create();
    prepare_pending(&fixture, "other_robot");
    let marker_bytes = read(&fixture.marker());

    let mut daemon = fixture.spawn_daemon(&fixture.latch());
    assert_eq!(daemon.wait_exit(FAILURE_EXIT_TIMEOUT_MS), 1);
    let log = daemon.log();
    assert!(log.contains("latch 启动恢复失败"), "{log}");
    assert_failed_before_adapter(&log);
    assert_eq!(read(&fixture.marker()), marker_bytes);
    assert_eq!(read(&fixture.latch()), b"0\n");
    assert_eq!(read(&fixture.backup()), b"0\n");
}

/// 9. IO 失败：双份 `1` 但 primary `mode 000` → 恢复读 primary 得 `EACCES`，退 1、日志
///    `latch 启动恢复失败`；既不回退 backup 也不自愈，恢复权限后双份仍 `1`。
#[test]
fn unreadable_primary_fails_recovery_and_keeps_pair() {
    // SAFETY：geteuid 无参数、无副作用、不会失败。
    let is_root = unsafe { libc::geteuid() } == 0;
    if is_root {
        // root 不受文件 mode 约束，mode 000 构造不出真实的读失败：root 下不做断言直接返回。
        return;
    }
    let fixture = Fixture::create();
    fixture.write_pair(b"1\n");
    let primary = fixture.latch();
    std::fs::set_permissions(&primary, std::fs::Permissions::from_mode(0o000)).unwrap();

    let mut daemon = fixture.spawn_daemon(&primary);
    let code = daemon.wait_exit(FAILURE_EXIT_TIMEOUT_MS);
    std::fs::set_permissions(&primary, std::fs::Permissions::from_mode(0o640)).unwrap();
    assert_eq!(code, 1);
    let log = daemon.log();
    assert!(log.contains("latch 启动恢复失败"), "{log}");
    assert_failed_before_adapter(&log);
    assert_eq!(read(&primary), b"1\n");
    assert_eq!(read(&fixture.backup()), b"1\n");
}

/// 10. primary 是符号链接（指向内容 `1` 的普通文件，backup 缺失）→ 持锁后的数据对象检查
///     拒绝：退 1、日志 `latch 数据对象检查失败`；backup 不被创建，链接与目标内容都不变。
#[test]
fn symlinked_primary_fails_object_check_without_creating_backup() {
    let fixture = Fixture::create();
    let target = fixture.dir().join("link_target");
    std::fs::write(&target, b"1\n").unwrap();
    std::os::unix::fs::symlink(&target, fixture.latch()).unwrap();

    let mut daemon = fixture.spawn_daemon(&fixture.latch());
    assert_eq!(daemon.wait_exit(FAILURE_EXIT_TIMEOUT_MS), 1);
    let log = daemon.log();
    assert!(log.contains("latch 数据对象检查失败"), "{log}");
    assert!(log.contains("InspectObject(Primary)"), "{log}");
    assert_failed_before_adapter(&log);
    let metadata = std::fs::symlink_metadata(fixture.latch()).unwrap();
    assert!(metadata.file_type().is_symlink());
    assert!(is_absent(&fixture.backup()), "backup 不得被创建");
    assert_eq!(read(&target), b"1\n");
}
