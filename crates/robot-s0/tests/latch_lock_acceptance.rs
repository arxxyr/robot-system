//! S48 A3d / W1：latch 目标稳定锁、共享名称校验与绑定目录 fd 数据对象检查的公开验收。
//!
//! 只走 `robot_s0::latch_lock` 公开 API（模块内单测已覆盖名称表、EINTR 注入与
//! 基本对象类型），本文件从进程、别名与 W0 联动角度取证，逐项对应批准合同的最小
//! 验收矩阵：
//!
//! 1. 同目录不同合法 basename 可并发持锁；同一目标互斥且失败竞争者不改 PID；
//! 2. 同 canonical 父目录的别名路径（符号链接目录、`..` 回绕）互斥；末段符号链接不解析；
//! 3. 锁只由 latch 目标决定，与任何其它目录（如信任目录）无关；
//! 4. `.bak` / `.lock` / marker / 两种临时名 / RX 固定名在副作用前拒绝，覆盖"无对应
//!    合法 primary 实例的旧派生文件"；非 UTF-8 合法名保留；
//! 5. 锁对象为 symlink / hardlink / FIFO / 目录时拒绝，primary `1` / backup `0` 原字节不变；
//! 6. 真实 `CLOEXEC`（ready 握手后确认活着的 exec 子进程不继承）、竞争者 PID、持有者退出后可重取；
//! 7. 现存 primary / backup 的 symlink、跨名 hardlink、特殊文件、目录拒绝且不自愈；
//!    真实缺失 / 损坏普通文件保留各入口原 fallback；
//! 8. 锁后父目录替换：写入只落旧目录，新目录无写入，W0 store 目录身份与锁不一致；
//! 9. `exit_code()`：Held → 89，名字 / IO / 身份 → 1。
//!
//! 子进程复用 `deploy_guard_kill` 的做法：用 `current_exe()` 重新执行本测试二进制，只跑
//! [`latch_lock_child`]，由环境变量决定其模式；所有子进程由 [`ChildGuard`] RAII 持有，
//! 成功 / 断言失败 / 超时路径都 `kill + wait`，等待均有界。

#![cfg(target_os = "linux")]

use std::ffi::{CString, OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use robot_s0::clear_estop_transaction::ClearTxnStore;
use robot_s0::latch_lock::{
    DataObjects, LATCH_LOCK_FAILURE_EXIT_CODE, LATCH_LOCK_HELD_EXIT_CODE, LatchLockError,
    LatchNameError, LatchObject, LatchTarget, LockStage, acquire_latch_lock, validate_latch_name,
    validate_latch_path,
};
use tempfile::TempDir;

/// 子进程模式：尝试取锁一次并以退出码报告（成功 0 / 被占用 89 / 其它 1）。
const CHILD_TRY_ENV: &str = "ROBOT_S0_LATCH_LOCK_CHILD_TRY";
/// 子进程模式：取锁后写 marker 通知父进程，然后一直持锁直到被 kill。
const CHILD_HOLD_ENV: &str = "ROBOT_S0_LATCH_LOCK_CHILD_HOLD";
/// HOLD 模式的 marker 路径。
const CHILD_MARKER_ENV: &str = "ROBOT_S0_LATCH_LOCK_CHILD_MARKER";
/// 子进程模式：不取锁，写 ready marker 后只保持存活（证明父进程的锁 fd 没有被继承）。
const CHILD_SLEEP_ENV: &str = "ROBOT_S0_LATCH_LOCK_CHILD_SLEEP";
/// 等待子进程 marker / 退出的上限。
const WAIT_LIMIT: Duration = Duration::from_secs(20);

const PRIMARY: &str = "estop_latch";

/// 子进程入口：父进程模式下（无环境变量）直接返回，是个空 case。
#[test]
fn latch_lock_child() {
    if let Some(path) = std::env::var_os(CHILD_TRY_ENV) {
        let code = match acquire_latch_lock(Path::new(&path)) {
            Ok(_lock) => 0,
            Err(LatchLockError::Held { .. }) => i32::from(LATCH_LOCK_HELD_EXIT_CODE),
            Err(_) => i32::from(LATCH_LOCK_FAILURE_EXIT_CODE),
        };
        std::process::exit(code);
    }
    if let Some(path) = std::env::var_os(CHILD_HOLD_ENV) {
        let marker = std::env::var_os(CHILD_MARKER_ENV).expect("HOLD 模式需要 marker 路径");
        let _lock = acquire_latch_lock(Path::new(&path)).expect("子进程取锁");
        std::fs::write(marker, b"held").expect("写 marker");
        loop {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    if std::env::var_os(CHILD_SLEEP_ENV).is_some() {
        // 真实进入睡眠模式之后才发 ready；父进程据此确认"活着的 exec 子进程"已存在。
        let marker = std::env::var_os(CHILD_MARKER_ENV).expect("SLEEP 模式需要 marker 路径");
        std::fs::write(marker, b"sleeping").expect("写 marker");
        loop {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// RAII 子进程守卫：无论成功、断言失败、ready 超时还是退出超时，离开作用域都
/// `kill + wait`，不留下持锁或睡眠的子进程；所有等待都有明确 deadline。
struct ChildGuard {
    child: Child,
    label: &'static str,
}

impl ChildGuard {
    fn spawn(label: &'static str, envs: &[(&str, &OsStr)]) -> Self {
        let mut command = Command::new(std::env::current_exe().expect("current_exe"));
        command
            .args([
                "--exact",
                "latch_lock_child",
                "--nocapture",
                "--test-threads",
                "1",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for (key, value) in envs {
            command.env(key, value);
        }
        let child = command.spawn().expect("spawn latch_lock child");
        Self { child, label }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    /// 仍在运行（`try_wait` 无退出状态）。
    fn is_alive(&mut self) -> bool {
        self.child.try_wait().expect("try_wait").is_none()
    }

    /// 有界等待退出并返回退出码；超时或被信号终止都先回收再 panic。
    fn wait_exit_code(&mut self, limit: Duration) -> i32 {
        let deadline = Instant::now() + limit;
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                return status
                    .code()
                    .unwrap_or_else(|| panic!("{}: 子进程被信号终止: {status}", self.label));
            }
            assert!(
                Instant::now() < deadline,
                "{}: 子进程 {} 未在 {limit:?} 内退出",
                self.label,
                self.child.id()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// 有界等待子进程写下 ready marker；子进程提前退出或超时都 panic（守卫负责回收）。
    fn wait_ready(&mut self, marker: &Path, limit: Duration) {
        let deadline = Instant::now() + limit;
        loop {
            if marker.exists() {
                return;
            }
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                panic!("{}: 子进程在 ready 之前退出: {status}", self.label);
            }
            assert!(
                Instant::now() < deadline,
                "{}: 子进程 {} 未在 {limit:?} 内 ready",
                self.label,
                self.child.id()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// 显式回收：仍在运行则 kill，然后 wait（SIGKILL 后 wait 立即返回）。
    fn kill_and_wait(&mut self) {
        if self.is_alive() {
            self.child.kill().expect("kill child");
        }
        self.child.wait().expect("wait child");
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// 子进程尝试取锁一次（有界等待），返回其退出码。
fn child_try(latch: &Path) -> i32 {
    let mut child = ChildGuard::spawn("try", &[(CHILD_TRY_ENV, latch.as_os_str())]);
    child.wait_exit_code(WAIT_LIMIT)
}

fn read_text(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("读取 {} 失败: {error}", path.display()))
}

fn entries(dir: &Path) -> Vec<OsString> {
    let mut names: Vec<OsString> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    names.sort();
    names
}

fn own_pid_line() -> String {
    format!("{}\n", std::process::id())
}

fn mkfifo(path: &Path) {
    let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY：路径以 NUL 结尾；mkfifo 只创建命名管道，不打开它。
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0, "mkfifo");
}

fn assert_held_by_self(error: &LatchLockError) {
    assert!(
        matches!(error, LatchLockError::Held { pid: Some(pid) } if *pid == std::process::id()),
        "{error:?}"
    );
    assert_eq!(error.exit_code(), LATCH_LOCK_HELD_EXIT_CODE);
}

fn assert_identity_error(error: &LatchLockError, object: LatchObject) {
    assert!(
        matches!(
            error,
            LatchLockError::Identity {
                stage: LockStage::InspectObject(actual),
                ..
            } if *actual == object
        ),
        "{error:?}"
    );
    assert_eq!(error.exit_code(), LATCH_LOCK_FAILURE_EXIT_CODE);
}

// ── 1 / 3：不同名字并发、同目标互斥、锁只由 latch 目标决定 ─────────────────────────

/// 同目录两个合法 basename 各自独立持锁；同一目标第二次取锁得 `Held{本进程 pid}`
/// 且失败竞争者不改锁文件 PID；锁只由 latch 目标决定，与任何其它目录无关。
#[test]
fn different_names_lock_independently_and_same_target_is_exclusive() {
    let dir = TempDir::new().unwrap();
    let first = dir.path().join(PRIMARY);
    let second = dir.path().join("gripper_latch");
    let first_lock = acquire_latch_lock(&first).expect("第一目标");
    let second_lock = acquire_latch_lock(&second).expect("同目录第二个合法名字并发持锁");
    assert_eq!(
        read_text(&dir.path().join("estop_latch.lock")),
        own_pid_line()
    );
    assert_eq!(
        read_text(&dir.path().join("gripper_latch.lock")),
        own_pid_line()
    );

    let error = acquire_latch_lock(&first).expect_err("同一目标必须互斥");
    assert_held_by_self(&error);
    assert_eq!(
        read_text(&dir.path().join("estop_latch.lock")),
        own_pid_line(),
        "失败竞争者不改 PID"
    );

    // 模拟两个不同的信任目录：它们与 latch 锁无关，锁文件只在 latch 父目录里。
    let trust_left = dir.path().join("trust_a");
    let trust_right = dir.path().join("trust_b");
    std::fs::create_dir(&trust_left).unwrap();
    std::fs::create_dir(&trust_right).unwrap();
    let error = acquire_latch_lock(&first).expect_err("不同信任目录不能绕过同一 latch 锁");
    assert_held_by_self(&error);
    assert!(entries(&trust_left).is_empty(), "信任目录不得出现锁文件");
    assert!(entries(&trust_right).is_empty(), "信任目录不得出现锁文件");

    drop(first_lock);
    drop(second_lock);
    drop(acquire_latch_lock(&first).expect("释放后可重取"));
    assert_eq!(
        entries(dir.path()),
        [
            OsString::from("estop_latch.lock"),
            OsString::from("gripper_latch.lock"),
            OsString::from("trust_a"),
            OsString::from("trust_b"),
        ],
        "锁文件永不删除，也不产生其它文件"
    );
}

// ── 2：别名互斥、末段符号链接不解析 ───────────────────────────────────────────

/// 符号链接目录路径与 `..` 回绕路径解析到同一 canonical 父目录，与原路径互斥；
/// 末段本身是符号链接时不解析末段（名字仍是 `estop_latch`），但数据对象检查拒绝它。
#[test]
fn aliases_of_the_same_canonical_parent_are_mutually_exclusive() {
    let dir = TempDir::new().unwrap();
    let robot = dir.path().join("robot");
    std::fs::create_dir(&robot).unwrap();
    std::fs::create_dir(robot.join("sub")).unwrap();
    std::os::unix::fs::symlink("robot", dir.path().join("alias")).unwrap();
    let direct = robot.join(PRIMARY);
    let via_symlink_dir = dir.path().join("alias").join(PRIMARY);
    let via_dotdot = robot.join("sub").join("..").join(PRIMARY);

    let canonical = robot.canonicalize().unwrap();
    for alias in [&direct, &via_symlink_dir, &via_dotdot] {
        let target = LatchTarget::resolve(alias).unwrap();
        assert_eq!(target.canonical_parent(), canonical, "{}", alias.display());
        assert_eq!(target.name_of(LatchObject::Primary), OsStr::new(PRIMARY));
        assert_eq!(target.lock_name(), OsStr::new("estop_latch.lock"));
    }

    let _held = acquire_latch_lock(&direct).expect("原路径持锁");
    for alias in [&via_symlink_dir, &via_dotdot] {
        let error = acquire_latch_lock(alias).expect_err("别名必须互斥");
        assert_held_by_self(&error);
    }
    assert_eq!(
        entries(&robot),
        [OsString::from("estop_latch.lock"), OsString::from("sub")],
        "别名不得产生第二个锁文件"
    );

    // 末段是符号链接：目标名字不解析（遵循 W0 目标身份规则），数据对象检查拒绝。
    let other = TempDir::new().unwrap();
    std::fs::write(other.path().join("real"), "1\n").unwrap();
    let linked = other.path().join(PRIMARY);
    std::os::unix::fs::symlink("real", &linked).unwrap();
    let lock = acquire_latch_lock(&linked).expect("末段符号链接不影响取锁");
    assert_eq!(
        lock.target().name_of(LatchObject::Primary),
        OsStr::new(PRIMARY)
    );
    assert_eq!(
        lock.target().canonical_parent(),
        other.path().canonicalize().unwrap()
    );
    let error = lock
        .check_data_objects()
        .expect_err("primary 为符号链接必须拒绝");
    assert_identity_error(&error, LatchObject::Primary);
    assert!(
        std::fs::symlink_metadata(&linked)
            .unwrap()
            .file_type()
            .is_symlink(),
        "不得替换或删除符号链接"
    );
    assert_eq!(read_text(&other.path().join("real")), "1\n");
    assert!(
        !other.path().join("estop_latch.bak").exists(),
        "不得自愈 backup"
    );
}

// ── 4：派生名 / RX 固定名在副作用前拒绝；非 UTF-8 合法名保留 ───────────────────

/// 派生尾缀、两种完整临时家族与 RX 固定名一律 `InvalidName`（退 1）且目录不出现新文件；
/// 目录中已有、但没有对应合法 primary 实例的旧派生文件也不能被当作 latch 目标，且其
/// 字节不变；非 UTF-8 合法名可以取锁、写入并读回。
#[test]
fn derived_and_reserved_names_are_rejected_before_any_side_effect() {
    let dir = TempDir::new().unwrap();
    let bad_names: [&[u8]; 9] = [
        b"estop_latch.bak",
        b"estop_latch.lock",
        b"estop_latch.clear_pending",
        b"estop_latch.tmp.1.2",
        b"estop_latch.clear_tmp.3.4",
        b"estop_latch.tmp.99999999999999999999999999.18446744073709551616",
        b"\xff\xfe.tmp.7.8",
        b"rx_trust_marker",
        b"rx_trust",
    ];
    for bad in bad_names {
        let name = OsStr::from_bytes(bad);
        assert!(validate_latch_name(name).is_err(), "{bad:?}");
        let path = dir.path().join(name);
        assert!(validate_latch_path(&path).is_err(), "{bad:?}");
        let error = acquire_latch_lock(&path).expect_err("必须拒绝");
        assert!(
            matches!(error, LatchLockError::InvalidName(_)),
            "{bad:?}: {error:?}"
        );
        assert_eq!(error.exit_code(), LATCH_LOCK_FAILURE_EXIT_CODE, "{bad:?}");
    }
    assert!(entries(dir.path()).is_empty(), "不得创建任何文件");

    // 旧派生文件已存在、但没有对应 primary 实例：仍不能作为 latch 目标，且字节不变。
    let stale = TempDir::new().unwrap();
    let stale_files: [(&str, &[u8]); 5] = [
        ("old.bak", b"1\n"),
        ("old.lock", b"4242\n"),
        ("old.clear_pending", b"pending"),
        ("old.tmp.1.2", b"1\n"),
        ("old.clear_tmp.3.4", b"0\n"),
    ];
    for (name, bytes) in stale_files {
        std::fs::write(stale.path().join(name), bytes).unwrap();
    }
    let before = entries(stale.path());
    for (name, bytes) in stale_files {
        let path = stale.path().join(name);
        let error = acquire_latch_lock(&path).expect_err(name);
        assert!(
            matches!(
                error,
                LatchLockError::InvalidName(
                    LatchNameError::ReservedSuffix(_) | LatchNameError::TemporaryFamily(_)
                )
            ),
            "{name}: {error:?}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes, "{name} 字节不变");
    }
    assert_eq!(entries(stale.path()), before, "不得新增派生文件");

    // 非 UTF-8 合法名：取锁、写入、读回都可用。
    let raw = TempDir::new().unwrap();
    let name = OsStr::from_bytes(b"\xff\xfe\x80latch");
    assert_eq!(validate_latch_name(name), Ok(()));
    let path = raw.path().join(name);
    let lock = acquire_latch_lock(&path).expect("非 UTF-8 合法名可取锁");
    lock.write_latch_strict(LatchObject::Primary, true).unwrap();
    lock.write_latch_strict(LatchObject::Backup, true).unwrap();
    assert!(lock.read_latch_value(LatchObject::Primary).unwrap());
    assert!(lock.read_latch_value(LatchObject::Backup).unwrap());
    let objects = lock.check_data_objects().unwrap();
    assert!(objects.primary.is_some() && objects.backup.is_some());
    let mut expected_lock = name.to_os_string();
    expected_lock.push(".lock");
    assert!(raw.path().join(&expected_lock).is_file());
}

// ── 5：锁对象类型 ────────────────────────────────────────────────────────────

/// 锁对象为 symlink / hardlink / FIFO / 目录 → 取锁失败（都不是 `Held`，退 1），且覆盖
/// primary `1` / backup `0` 这一有效崩溃状态的原字节不变；锁对象本身不被 rename / unlink。
#[test]
fn illegal_lock_objects_are_rejected_and_pair_bytes_are_untouched() {
    for variant in ["symlink", "hardlink", "fifo", "directory"] {
        let dir = TempDir::new().unwrap();
        let primary = dir.path().join(PRIMARY);
        let backup = dir.path().join("estop_latch.bak");
        std::fs::write(&primary, "1\n").unwrap();
        std::fs::write(&backup, "0\n").unwrap();
        let lock_path = dir.path().join("estop_latch.lock");
        match variant {
            "symlink" => {
                std::fs::write(dir.path().join("elsewhere"), "").unwrap();
                std::os::unix::fs::symlink("elsewhere", &lock_path).unwrap();
            }
            "hardlink" => {
                std::fs::write(&lock_path, "").unwrap();
                std::fs::hard_link(&lock_path, dir.path().join("elsewhere")).unwrap();
            }
            "fifo" => mkfifo(&lock_path),
            _ => std::fs::create_dir(&lock_path).unwrap(),
        }
        let type_before = std::fs::symlink_metadata(&lock_path).unwrap().file_type();
        let before = entries(dir.path());

        let error = acquire_latch_lock(&primary).expect_err(variant);
        assert!(
            !matches!(error, LatchLockError::Held { .. }),
            "{variant}: {error:?}"
        );
        assert!(
            matches!(
                error,
                LatchLockError::Io {
                    stage: LockStage::OpenLock,
                    ..
                } | LatchLockError::Identity {
                    stage: LockStage::InspectLock,
                    ..
                }
            ),
            "{variant}: {error:?}"
        );
        assert_eq!(error.exit_code(), LATCH_LOCK_FAILURE_EXIT_CODE, "{variant}");
        assert_eq!(read_text(&primary), "1\n", "{variant}: primary 原字节");
        assert_eq!(read_text(&backup), "0\n", "{variant}: backup 原字节");
        assert_eq!(
            std::fs::symlink_metadata(&lock_path).unwrap().file_type(),
            type_before,
            "{variant}: 锁对象不得被替换"
        );
        assert_eq!(entries(dir.path()), before, "{variant}: 目录条目不变");
    }
}

// ── 6：CLOEXEC、竞争者 PID、持有者退出后重取 ──────────────────────────────────

/// 锁 fd 带 `FD_CLOEXEC`：父进程持锁时子进程取锁得 89；父进程持锁期间拉起的
/// 长存活子进程（真实 ready 握手后确认存活）不会继承锁 fd——父进程放锁前后、另一
/// 进程重取前后 sleeper 都仍然活着，另一进程仍能立即取锁。
#[test]
fn lock_fd_is_not_inherited_across_exec() {
    let dir = TempDir::new().unwrap();
    let latch = dir.path().join(PRIMARY);
    let lock = acquire_latch_lock(&latch).unwrap();
    assert!(lock.lock_is_cloexec().unwrap(), "锁 fd 必须 CLOEXEC");

    assert_eq!(
        child_try(&latch),
        i32::from(LATCH_LOCK_HELD_EXIT_CODE),
        "父进程持锁时子进程必须得到 89"
    );
    assert_eq!(
        read_text(&dir.path().join("estop_latch.lock")),
        own_pid_line()
    );

    // 持锁期间拉起一个只保持存活的 exec 子进程，等它真实进入睡眠模式（ready marker）。
    // 若锁 fd 被它继承，父进程释放后锁仍会被这个活着的子进程持有。
    let ready = dir.path().join("sleeper_ready");
    let mut sleeper = ChildGuard::spawn(
        "sleeper",
        &[
            (CHILD_SLEEP_ENV, OsStr::new("1")),
            (CHILD_MARKER_ENV, ready.as_os_str()),
        ],
    );
    sleeper.wait_ready(&ready, WAIT_LIMIT);
    assert!(sleeper.is_alive(), "sleeper 必须在父进程放锁前仍活着");
    drop(lock);
    assert!(sleeper.is_alive(), "sleeper 必须在父进程放锁后仍活着");
    assert_eq!(
        child_try(&latch),
        0,
        "锁 fd 未被活着的 sleeper 继承：父进程释放后另一进程立即可取锁"
    );
    assert!(sleeper.is_alive(), "sleeper 必须在另一进程重取后仍活着");
    let reacquired = acquire_latch_lock(&latch).expect("父进程也可重取");
    assert!(sleeper.is_alive(), "sleeper 必须在父进程重取后仍活着");
    assert_eq!(
        read_text(&dir.path().join("estop_latch.lock")),
        own_pid_line()
    );
    drop(reacquired);
    sleeper.kill_and_wait();
    assert!(!sleeper.is_alive(), "sleeper 已回收");
}

/// 子进程持锁时父进程得到 `Held{pid: 子进程 pid}`；子进程被 kill 并回收后父进程可重取。
/// holder 由 RAII 守卫持有：ready 超时或任一断言失败都会被 kill + wait。
#[test]
fn competitor_pid_is_reported_and_lock_is_reacquirable_after_holder_exit() {
    let dir = TempDir::new().unwrap();
    let latch = dir.path().join(PRIMARY);
    let marker = dir.path().join("child_holds");
    let mut holder = ChildGuard::spawn(
        "holder",
        &[
            (CHILD_HOLD_ENV, latch.as_os_str()),
            (CHILD_MARKER_ENV, marker.as_os_str()),
        ],
    );
    let holder_pid = holder.pid();
    holder.wait_ready(&marker, WAIT_LIMIT);
    assert!(holder.is_alive(), "holder 必须仍持锁存活");

    let error = acquire_latch_lock(&latch).expect_err("子进程持锁时父进程必须失败");
    assert!(
        matches!(error, LatchLockError::Held { pid: Some(pid) } if pid == holder_pid),
        "{error:?} (holder pid {holder_pid})"
    );
    assert_eq!(error.exit_code(), LATCH_LOCK_HELD_EXIT_CODE);
    assert_eq!(
        read_text(&dir.path().join("estop_latch.lock")),
        format!("{holder_pid}\n"),
        "失败竞争者不改持有者 PID"
    );
    assert_eq!(
        child_try(&latch),
        i32::from(LATCH_LOCK_HELD_EXIT_CODE),
        "另一进程同样得到 89"
    );

    holder.kill_and_wait();
    assert!(!holder.is_alive(), "holder 已回收");
    let lock = acquire_latch_lock(&latch).expect("持有者退出后可重取");
    assert_eq!(
        read_text(&dir.path().join("estop_latch.lock")),
        own_pid_line()
    );
    drop(lock);
}

// ── 7：数据对象 ──────────────────────────────────────────────────────────────

/// 现存 primary / backup 的 symlink、跨不同名字的 hardlink、FIFO、目录都拒绝且不自愈
/// （不创建缺失的另一份、不改任何字节）；真实缺失记为 `None` 且读取得 `NotFound`；
/// 损坏普通文件通过类型检查、读取得 `InvalidData`（fallback 策略留给各入口）。
#[test]
fn data_object_aliases_and_special_files_are_rejected_without_self_heal() {
    // 真实缺失。
    let dir = TempDir::new().unwrap();
    let latch = dir.path().join(PRIMARY);
    let backup = dir.path().join("estop_latch.bak");
    let lock = acquire_latch_lock(&latch).unwrap();
    assert_eq!(
        lock.check_data_objects().unwrap(),
        DataObjects {
            primary: None,
            backup: None
        }
    );
    assert_eq!(
        lock.read_latch_value(LatchObject::Primary)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::NotFound
    );

    // 损坏普通文件：类型检查通过，读取 InvalidData。
    std::fs::write(&latch, "garbage").unwrap();
    let objects = lock.check_data_objects().unwrap();
    assert!(objects.primary.is_some() && objects.backup.is_none());
    assert_eq!(
        lock.read_latch_value(LatchObject::Primary)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::InvalidData
    );
    assert_eq!(read_text(&latch), "garbage", "损坏内容不被改写");
    assert!(!backup.exists(), "不得自愈 backup");
    drop(lock);

    // 跨不同名字的 hardlink：primary 的 nlink 变 2。
    let dir = TempDir::new().unwrap();
    let latch = dir.path().join(PRIMARY);
    std::fs::write(&latch, "1\n").unwrap();
    std::fs::hard_link(&latch, dir.path().join("other_name")).unwrap();
    let lock = acquire_latch_lock(&latch).unwrap();
    let error = lock
        .check_data_objects()
        .expect_err("跨名 hardlink 必须拒绝");
    assert_identity_error(&error, LatchObject::Primary);
    assert_eq!(
        std::fs::metadata(&latch).unwrap().nlink(),
        2,
        "不得 unlink 任一名字"
    );
    assert!(
        !dir.path().join("estop_latch.bak").exists(),
        "不得自愈 backup"
    );
    drop(lock);

    // backup 为符号链接 / FIFO / 目录（primary 合法）：拒绝且 primary 不动。
    for variant in ["symlink", "fifo", "directory"] {
        let dir = TempDir::new().unwrap();
        let latch = dir.path().join(PRIMARY);
        let backup = dir.path().join("estop_latch.bak");
        std::fs::write(&latch, "1\n").unwrap();
        match variant {
            "symlink" => std::os::unix::fs::symlink("estop_latch", &backup).unwrap(),
            "fifo" => mkfifo(&backup),
            _ => std::fs::create_dir(&backup).unwrap(),
        }
        let type_before = std::fs::symlink_metadata(&backup).unwrap().file_type();
        let lock = acquire_latch_lock(&latch).unwrap();
        let error = lock.check_data_objects().expect_err(variant);
        assert_identity_error(&error, LatchObject::Backup);
        assert_eq!(read_text(&latch), "1\n", "{variant}: primary 不动");
        assert_eq!(
            std::fs::symlink_metadata(&backup).unwrap().file_type(),
            type_before,
            "{variant}: 特殊对象不被替换"
        );
    }
}

// ── 8：锁后父目录替换 ─────────────────────────────────────────────────────────

/// 持锁后父目录被整体替换：绑定 fd 的写入只落旧目录、新目录无任何写入；按原路径打开的
/// W0 store 目录身份与锁不一致（daemon 据此拒绝启动）；未替换时身份一致。
#[test]
fn parent_replacement_after_lock_keeps_new_directory_untouched_and_breaks_store_identity() {
    let dir = TempDir::new().unwrap();
    let parent = dir.path().join("robot");
    std::fs::create_dir(&parent).unwrap();
    let latch = parent.join(PRIMARY);
    let lock = acquire_latch_lock(&latch).unwrap();

    // 未替换：store 与锁指向同一目录。
    let store = ClearTxnStore::open(&latch, "robot", "arm").unwrap();
    assert!(
        lock.directory_identity()
            .matches(store.directory_identity())
    );
    drop(store);

    // 替换：旧目录改名、新目录占用原路径。
    let moved = dir.path().join("robot.old");
    std::fs::rename(&parent, &moved).unwrap();
    std::fs::create_dir(&parent).unwrap();

    lock.write_latch_strict(LatchObject::Primary, true).unwrap();
    lock.write_latch_strict(LatchObject::Backup, true).unwrap();
    lock.sync_directory().unwrap();
    assert!(entries(&parent).is_empty(), "新目录不得收到任何写入");
    assert_eq!(
        entries(&moved),
        [
            OsString::from("estop_latch"),
            OsString::from("estop_latch.bak"),
            OsString::from("estop_latch.lock"),
        ]
    );
    assert_eq!(read_text(&moved.join(PRIMARY)), "1\n");

    let store = ClearTxnStore::open(&latch, "robot", "arm").unwrap();
    assert!(
        !lock
            .directory_identity()
            .matches(store.directory_identity()),
        "父目录替换后 store 目录身份必须与锁不一致"
    );
    assert!(entries(&parent).is_empty(), "打开 store 也不得在新目录写入");
}

// ── 9：退出码分区 ─────────────────────────────────────────────────────────────

/// 用真实错误实例核退出码：`Held` → 89；`InvalidName` / `Io` / `Identity` → 1。
#[test]
fn exit_codes_partition_held_from_configuration_failures() {
    let dir = TempDir::new().unwrap();
    let latch = dir.path().join(PRIMARY);
    let _held = acquire_latch_lock(&latch).unwrap();
    let held = acquire_latch_lock(&latch).unwrap_err();
    assert!(matches!(held, LatchLockError::Held { .. }));
    assert_eq!(held.exit_code(), 89);

    let invalid = acquire_latch_lock(&dir.path().join("estop_latch.bak")).unwrap_err();
    assert!(matches!(invalid, LatchLockError::InvalidName(_)));
    assert_eq!(invalid.exit_code(), 1);

    let io = acquire_latch_lock(&dir.path().join("missing").join(PRIMARY)).unwrap_err();
    assert!(matches!(
        io,
        LatchLockError::Io {
            stage: LockStage::CanonicalizeParent,
            ..
        }
    ));
    assert_eq!(io.exit_code(), 1);

    let other = TempDir::new().unwrap();
    let lock_path = other.path().join("estop_latch.lock");
    std::fs::write(&lock_path, "").unwrap();
    std::fs::hard_link(&lock_path, other.path().join("alias")).unwrap();
    let identity = acquire_latch_lock(&other.path().join(PRIMARY)).unwrap_err();
    assert!(matches!(
        identity,
        LatchLockError::Identity {
            stage: LockStage::InspectLock,
            ..
        }
    ));
    assert_eq!(identity.exit_code(), 1);
}
