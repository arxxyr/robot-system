//! v0.5 候选 C (5/5) `SetEnabled` cross-process e2e。
//!
//! ## 拓扑（D-P3：**不**起 s1）
//!
//! 每 case 起 **s0_daemon（真）+ FakeHeartbeatPublisher（按需，扮演 s1 心跳源）
//! + gateway（带 `--config`）**：
//!
//! - `SetEnabled` 走 gateway 第 6 条 rr_io_thread → s0 `set_enabled_rr_server`
//!   →`ipc_thread` §5.5b/§6.6b（多 tick 观测，D8）。
//! - **状态观测器 = `GetState`**（候选 B read plane）：s0 `pub_thread` 真发
//!   `JointState`（含 `s0_mode`）到 `/<robot_id>/s0/<hardware_id>/state`，
//!   gateway 常驻池转 proto `RobotState.mode` —— 跨进程断言 Safe/Ready/Enabled。
//! - enable 链路：`SetEnabled(true)` → `Event::Enable`（Safe→Ready）→ fake
//!   heartbeat 驱动 `Ready→Enabled.Active`（effectful `Enable(true)`）→ pending
//!   resolve → Ack。**无 heartbeat → 滞留 Ready → DeadlineExceeded**（AC-2b）。
//!
//! ## 串行
//!
//! 同 `state_rpc_e2e`：`GLOBAL_LOCK` 串行 + 每 case 唯一 `robot_id` + 独立
//! loopback port + 独立 forensic dir。
//!
//! ## 覆盖（Plan §P.2.5 + §P.4 PE4 调整）
//!
//! | AC | test fn |
//! |----|---------|
//! | AC-2 | `enable_happy_observes_enabled_active` |
//! | AC-2b | `enable_deadline_no_heartbeat_returns_deadline_exceeded` |
//! | AC-3 + AC-8(ii) | `disable_happy_to_safe_no_latch_change` |
//! | AC-4 | `enable_latch_blocked_failed_precondition` |
//! | AC-5 / AC-3c | `latched_disable_allowed_no_clear_latch`（latched+**Safe** 幂等路径，PE4②）|
//! | AC-6 | `dry_run_precondition_no_side_effect` |
//! | AC-7 | `viewer_denied_operator_allowed` |
//! | AC-8(i) + AC-8b | `auto_enable_false_stays_safe`（双 hardware 选非 first，PE4③）|
//! | AC-8(iii) | `cli_hardware_id_no_match_fail_fast` |
//! | D-P1 条 1 | `concurrent_second_resource_exhausted` |
//!
//! **AC-2c / AC-3b（adapter-fail）不在此**（PB10：mock_hard_rt 无可控 ack-fail
//! 开关）→ (3/5) `pending_adapter_fault_resolves_internal_adapter_error` 集成
//! unit 覆盖。**AC-9（Fault 拒 enable）不在此**（PE4①：cross-process 无
//! production Fault 触发器，同 PB10 根因）→ (2/5) `(Fault, Enable)` 拒 unit +
//! (3/5) dry-run/live 共用 evaluator 的 Fault arm 覆盖。

#![cfg(target_os = "linux")]
#![allow(clippy::doc_markdown)]

mod common;

use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tonic::Code;
use tonic::transport::{Channel, Endpoint};
use uuid::Uuid;

use common::client::{OPERATOR_TOKEN, VIEWER_TOKEN};
use common::fake_heartbeat::FakeHeartbeatPublisher;
use common::subprocess::{SubprocessHandle, build_gateway_command, locate_test_bin};
use robot_rpc::proto::robot_control_client::RobotControlClient;
use robot_rpc::proto::{AuthContext, CommandMeta, GetStateRequest, S0Mode, SetEnabledCommand};

/// 强制全部 `#[tokio::test]` 串行（双保险：跟 `--test-threads=1` 并存）。
static GLOBAL_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

static CASE_SEQ: AtomicU32 = AtomicU32::new(0);

const HW: &str = "arm";
const S0_READY_MARKER: &str = "set_enabled_rr_server started";
const GATEWAY_READY_MARKER: &str = "rr_io_threads started";
const MARKER_TIMEOUT: Duration = Duration::from_secs(5);
const READY_TIMEOUT: Duration = Duration::from_secs(5);
const MODE_TIMEOUT: Duration = Duration::from_secs(3);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);
const NEVER_HARDWARE: &str = "never_declared_hw";

// ============================================================================
// per-case 环境
// ============================================================================

/// case 环境拓扑开关。
struct CaseCfg {
    /// 选中 hardware 段的 `auto_enable`（`None` = 不写字段 → s0 缺省 true）。
    auto_enable: Option<bool>,
    /// 预置 latch 文件内容（`true` → "1\n"，s0 启动 estop_latched）。
    latched: bool,
    /// 是否起 fake heartbeat 源（enable 到 Enabled.Active 的必要条件）。
    heartbeat: bool,
    /// AC-8b：在选中段**之前**插一个 decoy `[[hardware]]`（auto_enable=true）；
    /// 若 s0 误读 first 段则行为可观测地不同。
    decoy_first_hardware: bool,
}

struct SetEnabledEnv {
    channel: Channel,
    s0: Option<SubprocessHandle>,
    gateway: Option<SubprocessHandle>,
    heartbeat: Option<FakeHeartbeatPublisher>,
    latch_path: PathBuf,
    forensic_dir: PathBuf,
}

impl SetEnabledEnv {
    async fn bring_up(test_name: &str, cfg: CaseCfg) -> Self {
        let pid = std::process::id();
        let seq = CASE_SEQ.fetch_add(1, Ordering::Relaxed);
        let robot_id = format!("set_enabled_e2e_{pid}_{seq}_{test_name}");
        let forensic_dir = PathBuf::from(format!("/tmp/set_enabled_e2e/{robot_id}"));
        std::fs::create_dir_all(&forensic_dir).expect("create forensic dir");

        let listen_addr = alloc_loopback_port();

        // latch 预置（s0 启动读取；"1\n" → estop_latched=true）。
        let latch_path = forensic_dir.join("estop_latch");
        std::fs::write(&latch_path, if cfg.latched { "1\n" } else { "0\n" }).expect("preset latch");

        // 共用 config（s0：adapter/auto_enable 选段；gateway：state 池 hardware 声明）。
        let config_path = forensic_dir.join("robot.toml");
        std::fs::write(
            &config_path,
            build_config_toml(&robot_id, cfg.auto_enable, cfg.decoy_first_hardware),
        )
        .expect("write config");

        // rpc_roles.toml（复用 workspace configs/rpc_roles.toml：set_enabled =
        // operator+，viewer 无）。
        let roles_path = forensic_dir.join("rpc_roles.toml");
        let roles_text =
            std::fs::read_to_string(workspace_rpc_roles_path()).expect("read workspace rpc_roles");
        std::fs::write(&roles_path, roles_text).expect("write rpc_roles");

        // ── 1) s0_daemon ──
        let s0_bin = locate_test_bin("s0_daemon").expect("locate s0_daemon bin");
        let mut s0 = SubprocessHandle::spawn(
            "s0",
            build_s0_command(&s0_bin, &robot_id, &config_path, &latch_path),
        )
        .expect("spawn s0_daemon");
        if let Err(e) = s0.wait_for_marker(S0_READY_MARKER, MARKER_TIMEOUT).await {
            for line in s0.snapshot_head_tail(30).await {
                eprintln!("[set_enabled_e2e s0] {line}");
            }
            panic!("s0 ready marker timeout: {e}");
        }

        // ── 2) fake heartbeat（按需）──
        let heartbeat = if cfg.heartbeat {
            Some(FakeHeartbeatPublisher::spawn(&robot_id).expect("spawn FakeHeartbeatPublisher"))
        } else {
            None
        };

        // ── 3) gateway ──
        let gateway_bin = locate_test_bin("robot_rpc_gateway").expect("locate gateway bin");
        let mut gateway = SubprocessHandle::spawn(
            "gateway",
            build_gateway_command(
                &gateway_bin,
                &robot_id,
                listen_addr,
                &roles_path,
                Some(&config_path),
            ),
        )
        .expect("spawn gateway");
        if let Err(e) = gateway
            .wait_for_marker(GATEWAY_READY_MARKER, MARKER_TIMEOUT)
            .await
        {
            for line in gateway.snapshot_head_tail(30).await {
                eprintln!("[set_enabled_e2e gateway] {line}");
            }
            panic!("gateway ready marker timeout: {e}");
        }

        // ── 4) readiness：connect + GetState(NEVER)→NotFound + s0 state 首帧 ──
        let channel = connect_and_probe(listen_addr).await;
        let env = Self {
            channel,
            s0: Some(s0),
            gateway: Some(gateway),
            heartbeat,
            latch_path,
            forensic_dir,
        };
        // s0 pub_thread 首帧（GetState(HW) 直到 Ok）= 全链路 ready。
        let _ = env.mode_now().await;
        env
    }

    fn client(&self) -> RobotControlClient<Channel> {
        RobotControlClient::new(self.channel.clone())
    }

    /// 读当前 `RobotState.mode`（吸收首帧前短暂 Unavailable）。
    async fn mode_now(&self) -> i32 {
        let mut client = self.client();
        let deadline = tokio::time::Instant::now() + MODE_TIMEOUT;
        loop {
            match client
                .get_state(GetStateRequest {
                    hardware_id: HW.to_string(),
                })
                .await
            {
                Ok(resp) => return resp.into_inner().mode,
                Err(s)
                    if s.code() == Code::Unavailable && tokio::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(s) => panic!("get_state({HW}): {:?} {}", s.code(), s.message()),
            }
        }
    }

    /// 轮询 GetState 直到 `mode == want`（s0 50Hz publish 异步收敛）。
    async fn wait_mode(&self, want: S0Mode) {
        let deadline = tokio::time::Instant::now() + MODE_TIMEOUT;
        loop {
            let mode = self.mode_now().await;
            if mode == want as i32 {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "wait_mode({want:?}) timeout；最后观测 mode={mode}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn latch_bytes(&self) -> String {
        std::fs::read_to_string(&self.latch_path).expect("read latch file")
    }

    async fn shutdown(mut self) {
        if let Some(gw) = self.gateway.take() {
            let status = gw.shutdown(SHUTDOWN_GRACE).await.expect("gateway shutdown");
            assert!(status.success(), "gateway exit != 0: {status:?}");
        }
        if let Some(s0) = self.s0.take() {
            let status = s0.shutdown(SHUTDOWN_GRACE).await.expect("s0 shutdown");
            assert!(status.success(), "s0 exit != 0: {status:?}");
        }
        if let Some(hb) = self.heartbeat.take() {
            hb.shutdown();
        }
        let _ = std::fs::remove_dir_all(&self.forensic_dir);
    }
}

// ============================================================================
// helpers
// ============================================================================

fn alloc_loopback_port() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind 0");
    let addr = listener.local_addr().expect("local_addr");
    drop(listener);
    addr
}

fn build_config_toml(robot_id: &str, auto_enable: Option<bool>, decoy_first: bool) -> String {
    use std::fmt::Write;
    let mut s = format!(
        "# v0.5 候选 C (5/5) set_enabled_e2e 共用 config（s0 + gateway）\n\
         [robot]\nid = \"{robot_id}\"\nconfig_version = \"0.5.0\"\n\n"
    );
    if decoy_first {
        // AC-8b decoy：first 段 auto_enable=true；s0 --hardware-id 选非 first。
        let _ = write!(
            s,
            "[[hardware]]\nid = \"decoy_arm\"\nadapter = \"mock_hard_rt\"\n\
             joints = [\"j0\", \"j1\"]\nmotor_ids = [1, 2]\nauto_enable = true\n\n"
        );
    }
    let _ = write!(
        s,
        "[[hardware]]\nid = \"{HW}\"\nadapter = \"mock_hard_rt\"\njoints = [\"j0\", \"j1\"]\nmotor_ids = [1, 2]\n"
    );
    if let Some(v) = auto_enable {
        let _ = writeln!(s, "auto_enable = {v}");
    }
    s.push('\n');
    s
}

fn build_s0_command(
    bin_path: &Path,
    robot_id: &str,
    config_path: &Path,
    latch_path: &Path,
) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(bin_path);
    cmd.arg("--robot-id")
        .arg(robot_id)
        .arg("--hardware-id")
        .arg(HW)
        .arg("--config")
        .arg(config_path)
        .arg("--latch-path")
        .arg(latch_path)
        .arg("--tick-hz")
        .arg("50")
        // 兜底防 runaway（SIGTERM 正常路径远早于此）。
        .arg("--smoke-duration")
        .arg("120s");
    cmd.env("RUST_LOG", "info");
    cmd
}

fn workspace_rpc_roles_path() -> PathBuf {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let path = PathBuf::from(manifest)
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .join("configs/rpc_roles.toml");
    assert!(path.exists(), "{} 不存在", path.display());
    path
}

/// connect tonic Channel + GetState(NEVER) → NotFound readiness probe（沿
/// state_rpc_e2e）。
async fn connect_and_probe(listen_addr: SocketAddr) -> Channel {
    let endpoint =
        Endpoint::from_shared(format!("http://{listen_addr}")).expect("tonic endpoint from addr");
    let deadline = tokio::time::Instant::now() + READY_TIMEOUT;

    let channel = loop {
        match endpoint.connect().await {
            Ok(c) => break c,
            Err(e) => {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "tonic connect failed within {READY_TIMEOUT:?}: {e}"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    };

    let mut client = RobotControlClient::new(channel.clone());
    loop {
        let resp = client
            .get_state(GetStateRequest {
                hardware_id: NEVER_HARDWARE.to_string(),
            })
            .await;
        match resp {
            Err(s) if s.code() == Code::NotFound => break,
            other => assert!(
                tokio::time::Instant::now() < deadline,
                "readiness GetState(NEVER) probe never returned NotFound: {other:?}"
            ),
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    channel
}

/// 构造 `SetEnabledCommand`。`request_id` 每次新 UUIDv7；`idempotency_key` 由
/// case 指定（case 内唯一）。
fn make_cmd(
    idempotency_key: &str,
    enabled: bool,
    dry_run: bool,
    timeout_ms: u64,
    token: &str,
) -> SetEnabledCommand {
    SetEnabledCommand {
        meta: Some(CommandMeta {
            request_id: Uuid::now_v7().to_string(),
            source: "e2e.set_enabled".into(),
            auth: Some(AuthContext {
                token: token.into(),
                mtls_fingerprint: String::new(),
            }),
            timeout_ms,
            idempotency_key: idempotency_key.into(),
            dry_run,
            audit_reason: "set_enabled_e2e".into(),
        }),
        enabled,
    }
}

// ============================================================================
// AC-2：enable happy —— Safe → SetEnabled(true) + heartbeat → Enabled.Active
// ============================================================================

#[tokio::test]
async fn enable_happy_observes_enabled_active() {
    let _guard = GLOBAL_LOCK.lock().await;
    let env = SetEnabledEnv::bring_up(
        "enhappy",
        CaseCfg {
            auto_enable: Some(false),
            latched: false,
            heartbeat: true,
            decoy_first_hardware: false,
        },
    )
    .await;
    let mut client = env.client();

    // 前置：auto_enable=false → 即使 heartbeat 在跑也停在 SAFE。
    assert_eq!(env.mode_now().await, S0Mode::Safe as i32, "前置应 SAFE");

    // SetEnabled(true) → D8：观测到 Enabled.Active 才 Ack。
    let ack = client
        .set_enabled(make_cmd("idem-enhappy", true, false, 3_000, OPERATOR_TOKEN))
        .await
        .expect("enable happy 应 Ack")
        .into_inner();
    assert!(ack.cleared_at_ns > 0, "completed_at_ns 应非零");
    assert!(!ack.dry_run);

    // RPC success ⟺ 状态机已到 Enabled（AC-2 锁）。
    env.wait_mode(S0Mode::Enabled).await;

    env.shutdown().await;
}

// ============================================================================
// AC-2b：enable 无 heartbeat → 到 Ready 后 deadline → DeadlineExceeded
// ============================================================================

#[tokio::test]
async fn enable_deadline_no_heartbeat_returns_deadline_exceeded() {
    let _guard = GLOBAL_LOCK.lock().await;
    let env = SetEnabledEnv::bring_up(
        "endl",
        CaseCfg {
            auto_enable: Some(false),
            latched: false,
            heartbeat: false,
            decoy_first_hardware: false,
        },
    )
    .await;
    let mut client = env.client();

    let err = client
        .set_enabled(make_cmd("idem-endl", true, false, 1_200, OPERATOR_TOKEN))
        .await
        .expect_err("无 heartbeat 应 DeadlineExceeded");
    assert_eq!(err.code(), Code::DeadlineExceeded);

    // D8：deadline **不**回滚副作用 —— Safe→Ready 已发生，留在 READY。
    env.wait_mode(S0Mode::Ready).await;

    env.shutdown().await;
}

// ============================================================================
// AC-3 + AC-8(ii)：disable happy —— auto-enable（缺省 true）到 Enabled 后
// SetEnabled(false) → Safe；latch 文件 0 改
// ============================================================================

#[tokio::test]
async fn disable_happy_to_safe_no_latch_change() {
    let _guard = GLOBAL_LOCK.lock().await;
    let env = SetEnabledEnv::bring_up(
        "dishappy",
        CaseCfg {
            auto_enable: None, // 缺省 = true（AC-8ii：既有启动 auto-enable 行为）
            latched: false,
            heartbeat: true,
            decoy_first_hardware: false,
        },
    )
    .await;
    let mut client = env.client();

    // AC-8(ii)：auto_enable 缺省 → 启动自动 enable + heartbeat → ENABLED。
    env.wait_mode(S0Mode::Enabled).await;
    let latch_before = env.latch_bytes();
    assert_eq!(latch_before.trim(), "0");

    let ack = client
        .set_enabled(make_cmd(
            "idem-dishappy",
            false,
            false,
            3_000,
            OPERATOR_TOKEN,
        ))
        .await
        .expect("disable happy 应 Ack")
        .into_inner();
    assert!(ack.cleared_at_ns > 0);

    env.wait_mode(S0Mode::Safe).await;
    // AC-3：disable 不清/不置 latch —— 文件逐字节不变。
    assert_eq!(env.latch_bytes(), latch_before, "latch 文件被 disable 改动");

    env.shutdown().await;
}

// ============================================================================
// AC-4（§6.7 #3）：estop_latched → SetEnabled(true) 拒 FailedPrecondition；
// latch 文件不变
// ============================================================================

#[tokio::test]
async fn enable_latch_blocked_failed_precondition() {
    let _guard = GLOBAL_LOCK.lock().await;
    let env = SetEnabledEnv::bring_up(
        "latchblk",
        CaseCfg {
            auto_enable: Some(false),
            latched: true,
            heartbeat: false,
            decoy_first_hardware: false,
        },
    )
    .await;
    let mut client = env.client();

    let err = client
        .set_enabled(make_cmd(
            "idem-latchblk",
            true,
            false,
            2_000,
            OPERATOR_TOKEN,
        ))
        .await
        .expect_err("latched enable 应拒");
    assert_eq!(err.code(), Code::FailedPrecondition);

    // 留 SAFE + latch 文件不变（仍 "1"）。
    assert_eq!(env.mode_now().await, S0Mode::Safe as i32);
    assert_eq!(
        env.latch_bytes().trim(),
        "1",
        "latch 文件不得被 enable 改动"
    );

    env.shutdown().await;
}

// ============================================================================
// AC-5 / AC-3c（PE4②）：latched + Safe → SetEnabled(false) 幂等 Ack；latch
// 仍 "1"（不清）。"latched + Enabled" 非 cross-process 可达稳态（EStop 原子
// latch+降 Safe）；guard-independence 由 (2/5) state-machine unit 锁。
// ============================================================================

#[tokio::test]
async fn latched_disable_allowed_no_clear_latch() {
    let _guard = GLOBAL_LOCK.lock().await;
    let env = SetEnabledEnv::bring_up(
        "latchdis",
        CaseCfg {
            auto_enable: Some(false),
            latched: true,
            heartbeat: false,
            decoy_first_hardware: false,
        },
    )
    .await;
    let mut client = env.client();

    // RS1/AC-3c：Safe（已 torque-off 终态）+ disable → 即时幂等 Disabled(Ok)。
    let ack = client
        .set_enabled(make_cmd(
            "idem-latchdis",
            false,
            false,
            2_000,
            OPERATOR_TOKEN,
        ))
        .await
        .expect("latched disable 应 Ack（幂等）")
        .into_inner();
    assert!(ack.cleared_at_ns > 0);

    assert_eq!(env.mode_now().await, S0Mode::Safe as i32);
    // D3e/AC-5 锁：disable **不**清 latch。
    assert_eq!(env.latch_bytes().trim(), "1", "disable 不得清 latch");

    env.shutdown().await;
}

// ============================================================================
// AC-6（D7/B4）：dry_run=true 端到端进 S0 evaluate-only；0 state change；
// wire 带 dry_run（gateway 不丢弃）
// ============================================================================

#[tokio::test]
async fn dry_run_precondition_no_side_effect() {
    let _guard = GLOBAL_LOCK.lock().await;
    let env = SetEnabledEnv::bring_up(
        "dryrun",
        CaseCfg {
            auto_enable: Some(false),
            latched: false,
            heartbeat: false, // precondition 评估不依赖 heartbeat
            decoy_first_hardware: false,
        },
    )
    .await;
    let mut client = env.client();

    assert_eq!(env.mode_now().await, S0Mode::Safe as i32);

    // dry_run=true → S0 read-only 评估 WouldAccept → Ok；Ack.dry_run 回传 true
    // = wire 真携带 dry_run（**非** gateway 丢弃后实发动作的"假 dry-run"）。
    let ack = client
        .set_enabled(make_cmd("idem-dryrun", true, true, 2_000, OPERATOR_TOKEN))
        .await
        .expect("dry_run precondition 应 Ok")
        .into_inner();
    assert!(ack.dry_run, "Ack.dry_run 应回传 true（wire 带 dry_run）");

    // 0 state change：等 300ms（>10 个 s0 tick）后仍 SAFE（未发 Event::Enable）。
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        env.mode_now().await,
        S0Mode::Safe as i32,
        "dry_run 不得改 state"
    );
    assert_eq!(env.latch_bytes().trim(), "0", "dry_run 不得碰 latch");

    env.shutdown().await;
}

// ============================================================================
// AC-7（D2）：viewer → PermissionDenied；operator → 准（dry_run 无副作用探针）
// ============================================================================

#[tokio::test]
async fn viewer_denied_operator_allowed() {
    let _guard = GLOBAL_LOCK.lock().await;
    let env = SetEnabledEnv::bring_up(
        "auth",
        CaseCfg {
            auto_enable: Some(false),
            latched: false,
            heartbeat: false,
            decoy_first_hardware: false,
        },
    )
    .await;
    let mut client = env.client();

    let err = client
        .set_enabled(make_cmd("idem-auth-v", true, false, 2_000, VIEWER_TOKEN))
        .await
        .expect_err("viewer 应 PermissionDenied");
    assert_eq!(err.code(), Code::PermissionDenied);

    // operator → 准（用 dry_run 探针避免副作用）。
    let ack = client
        .set_enabled(make_cmd("idem-auth-o", true, true, 2_000, OPERATOR_TOKEN))
        .await
        .expect("operator 应准")
        .into_inner();
    assert!(ack.dry_run);

    env.shutdown().await;
}

// ============================================================================
// AC-8(i) + AC-8b（PE4③）：双 hardware（decoy first auto_enable=true + 选中
// 非 first =false）→ 启动留 SAFE（heartbeat 在跑也不 enable）。若 s0 误读
// first 段 auto_enable → 会 ENABLED → 本测 fail。
// ============================================================================

#[tokio::test]
async fn auto_enable_false_stays_safe() {
    let _guard = GLOBAL_LOCK.lock().await;
    let env = SetEnabledEnv::bring_up(
        "ae8b",
        CaseCfg {
            auto_enable: Some(false),
            latched: false,
            heartbeat: true,
            decoy_first_hardware: true,
        },
    )
    .await;

    // 观察 600ms（30 个 s0 tick + heartbeat 持续）：必须一直 SAFE。
    let deadline = tokio::time::Instant::now() + Duration::from_millis(600);
    while tokio::time::Instant::now() < deadline {
        assert_eq!(
            env.mode_now().await,
            S0Mode::Safe as i32,
            "auto_enable=false（matched 段）必须留 SAFE；若 ENABLED = 误读 first 段（AC-8b 破）"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    env.shutdown().await;
}

// ============================================================================
// AC-8(iii)：CLI --hardware-id 无匹配 [[hardware]].id → 启动 fail-fast exit 2
// ============================================================================

#[tokio::test]
async fn cli_hardware_id_no_match_fail_fast() {
    let _guard = GLOBAL_LOCK.lock().await;
    let pid = std::process::id();
    let seq = CASE_SEQ.fetch_add(1, Ordering::Relaxed);
    let robot_id = format!("set_enabled_e2e_{pid}_{seq}_failfast");
    let dir = PathBuf::from(format!("/tmp/set_enabled_e2e/{robot_id}"));
    std::fs::create_dir_all(&dir).expect("create dir");
    let latch_path = dir.join("estop_latch");
    std::fs::write(&latch_path, "0\n").expect("preset latch");
    let config_path = dir.join("robot.toml");
    std::fs::write(&config_path, build_config_toml(&robot_id, None, false)).expect("write config");

    let s0_bin = locate_test_bin("s0_daemon").expect("locate s0_daemon bin");
    let mut cmd = tokio::process::Command::new(&s0_bin);
    cmd.arg("--robot-id")
        .arg(&robot_id)
        .arg("--hardware-id")
        .arg("ghost_not_in_config") // ≠ config 内任何 [[hardware]].id
        .arg("--config")
        .arg(&config_path)
        .arg("--latch-path")
        .arg(&latch_path)
        .arg("--smoke-duration")
        .arg("30s")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);

    let mut child = cmd.spawn().expect("spawn s0_daemon");
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("s0 应在 5s 内 fail-fast 退出（而非常驻运行）")
        .expect("wait s0");
    assert_eq!(
        status.code(),
        Some(2),
        "config/CLI hardware mismatch 应 exit 2（fail-fast），实际 {status:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ============================================================================
// D-P1 条 1：并发第二个 live SetEnabled → ResourceExhausted（单飞）
// ============================================================================

#[tokio::test]
async fn concurrent_second_resource_exhausted() {
    let _guard = GLOBAL_LOCK.lock().await;
    let env = SetEnabledEnv::bring_up(
        "conc",
        CaseCfg {
            auto_enable: Some(false),
            latched: false,
            heartbeat: false, // 第一个滞留 Ready 观测期占住单飞槽
            decoy_first_hardware: false,
        },
    )
    .await;

    // A：长 deadline 占槽（无 heartbeat → 永到不了 Enabled.Active）。
    let mut client_a = env.client();
    let task_a = tokio::spawn(async move {
        client_a
            .set_enabled(make_cmd("idem-conc-A", true, false, 5_000, OPERATOR_TOKEN))
            .await
    });

    // **可观测 gate（非固定 sleep）**：等 GetState 观测到 READY —— Safe→Ready
    // 由 A 的 `Event::Enable` 驱动（本 env 无其它 Enable 源：auto_enable=false
    // + 无 heartbeat），READY 即 "A 已被 S0 接收并进入 enable pending" 的
    // 跨进程证据。满载 / release-debug 时差下不靠运气。
    env.wait_mode(S0Mode::Ready).await;
    assert!(
        !task_a.is_finished(),
        "A 不应在 B 之前完成（5s deadline 占槽中）"
    );

    // B：不同 idempotency_key → 不走 gateway replay/conflict，直达 S0 单飞拒。
    let mut client_b = env.client();
    let err_b = client_b
        .set_enabled(make_cmd("idem-conc-B", true, false, 1_000, OPERATOR_TOKEN))
        .await
        .expect_err("第二个并发 live 应 ResourceExhausted");
    assert_eq!(err_b.code(), Code::ResourceExhausted);

    // A 最终 DeadlineExceeded（释放单飞；不残留）。
    let err_a = task_a
        .await
        .expect("task_a join")
        .expect_err("A 无 heartbeat 应 DeadlineExceeded");
    assert_eq!(err_a.code(), Code::DeadlineExceeded);

    env.shutdown().await;
}
