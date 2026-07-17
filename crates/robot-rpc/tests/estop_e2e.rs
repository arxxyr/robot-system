//! v0.5 候选 D (2/3) `EStop` 触发 RPC cross-process e2e。
//!
//! ## 拓扑
//!
//! 每 case 起 **s0_daemon（真）+ gateway + EStopTopicCounter（harness 订阅
//! 计数线程）+ FakeHeartbeatPublisher（按需）**：
//!
//! - `EStop` 走 gateway publish `/<robot_id>/events/estop`（D1 路由 (a)，
//!   estop_pub_thread）→ s0 既有 poll_estop → emergency drain → safety_stop
//!   + latch fsync 链（**S0 0 改**）。
//! - **运行态观测器 = `GetState`**（mode / safety_state）；**latch 文件 "1"
//!   断言 = harness 本地读文件**（远程不可观测，CodeMap B2 口径）。
//! - **事件计数器 = EStopTopicCounter**（PD1）：订阅者在**第一次 RPC 之前**
//!   attach（bring_up 内最先建）；专用 std::thread 持 `!Send` subscriber，
//!   **`try_recv` 逐条 drain 计数（禁 `latest()`——塌缩多条事件造成假阳性）**；
//!   每条记录 (reason, source, latched, envelope source_id) 供 wire 字段断言。
//!
//! ## 串行
//!
//! 同 `set_enabled_e2e`：`GLOBAL_LOCK` 串行 + 每 case 唯一 `robot_id` + 独立
//! loopback port + 独立 forensic dir。
//!
//! ## 覆盖（Plan §P.2.2）
//!
//! | AC | test fn |
//! |----|---------|
//! | AC-2 + AC-8 + 候选 C AC-4 闭环 | `estop_from_safe_happy` |
//! | AC-3 | `estop_from_enabled_overrides` |
//! | AC-4 / PB3 | `viewer_denied_operator_maintainer_admin_allowed` |
//! | AC-5 / PD1 | `idem_replay_single_publish` |
//! | AC-6 / D4 | `dry_run_rejected` |
//! | AC-7 / PD2 / PB1 | `s0_dead_still_acks` |
//!
//! AC-9（docs）在 (3/3)；AC-1 = 既有 e2e/smoke 0 退化（AC-Q 统跑，含 PE3
//! 口径 clear_estop_e2e + robotctl_smoke）。

#![cfg(target_os = "linux")]
#![allow(clippy::doc_markdown)]

mod common;

use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tonic::Code;
use tonic::transport::{Channel, Endpoint};
use uuid::Uuid;

use common::client::{ADMIN_TOKEN, MAINTAINER_TOKEN, OPERATOR_TOKEN, VIEWER_TOKEN};
use common::fake_heartbeat::FakeHeartbeatPublisher;
use common::subprocess::{SubprocessHandle, build_gateway_command, locate_test_bin};
use robot_rpc::proto::robot_control_client::RobotControlClient;
use robot_rpc::proto::{
    AuthContext, CommandMeta, EStopCommand, GetStateRequest, S0Mode, S0SafetyState,
    SetEnabledCommand,
};

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
/// 事件计数 settle 窗口（counter 线程 5ms 轮询，200ms >> 传播时延）。
const COUNT_SETTLE: Duration = Duration::from_millis(200);
const NEVER_HARDWARE: &str = "never_declared_hw";

// ============================================================================
// EStopTopicCounter：harness 订阅计数线程（PD1）
// ============================================================================

/// 单条 EStopEvent 的记录（wire 字段断言用）。
#[derive(Debug, Clone)]
struct EStopRecord {
    reason: String,
    source: String,
    latched: bool,
    envelope_source_id: u32,
}

/// 专用 std::thread 持 `!Send` subscriber，`try_recv` 逐条 drain（禁
/// `latest()`）→ 记录 push 进共享 Vec。**必须在第一次 RPC 之前 spawn**
/// （PD1：订阅者先建好，事件不漏）。
struct EStopTopicCounter {
    records: Arc<Mutex<Vec<EStopRecord>>>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl EStopTopicCounter {
    fn spawn(robot_id: &str) -> Self {
        use iceoryx2::prelude::NodeBuilder;
        use iceoryx2::service::ipc::Service as IxService;
        use robot_msg::markers;
        use robot_transport::iceoryx2::{TopicClass, subscriber as open_sub};

        let topic = format!("/{robot_id}/events/estop");
        let records: Arc<Mutex<Vec<EStopRecord>>> = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let records_clone = Arc::clone(&records);
        let stop_clone = Arc::clone(&stop);

        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<()>(0);
        let handle = std::thread::Builder::new()
            .name("estop_counter".into())
            .spawn(move || {
                let node = NodeBuilder::new()
                    .create::<IxService>()
                    .expect("counter NodeBuilder create");
                let mut sub =
                    open_sub::<markers::EStopEvent>(&node, &topic, TopicClass::SafetyEvent)
                        .expect("counter open estop subscriber");
                Box::leak(Box::new(node));
                let _ = ready_tx.send(());
                drop(ready_tx);

                while !stop_clone.load(Ordering::Acquire) {
                    // PD1：逐条 try_recv drain 计数；**禁 latest()**。
                    while let Ok(Some(lease)) = sub.try_recv() {
                        if let Ok(msg) = lease.view() {
                            let ev = msg.body.get();
                            records_clone.lock().unwrap().push(EStopRecord {
                                reason: ev.reason().unwrap_or("").to_string(),
                                source: ev.source().unwrap_or("").to_string(),
                                latched: ev.latched(),
                                envelope_source_id: msg.header.source_id(),
                            });
                        }
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                // subscriber 正常 Drop。本线程在 gateway（publisher）退出之后才
                // join（shutdown 顺序 gateway 先退、counter 后退），此 Drop 即
                // "publisher 先退→存活端 drop subscriber"路径——0.8.1 时代此处
                // `mem::forget` 规避 SIGSEGV，iceoryx2 0.9.3〔上游 #1629 同根因
                // 修复〕升级后撤销，本 drop 兼作该崩溃类的回归探针。
            })
            .expect("spawn estop_counter");
        ready_rx
            .recv()
            .expect("estop_counter factory ready handshake");

        Self {
            records,
            stop,
            handle: Some(handle),
        }
    }

    fn snapshot(&self) -> Vec<EStopRecord> {
        self.records.lock().unwrap().clone()
    }

    fn shutdown(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for EStopTopicCounter {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

// ============================================================================
// per-case 环境（沿 set_enabled_e2e）
// ============================================================================

struct CaseCfg {
    /// 选中 hardware 段 `auto_enable`（`None` = 缺省 true）。
    auto_enable: Option<bool>,
    /// 是否起 fake heartbeat（enabled 态需要）。
    heartbeat: bool,
}

struct EStopEnv {
    channel: Channel,
    s0: Option<SubprocessHandle>,
    gateway: Option<SubprocessHandle>,
    heartbeat: Option<FakeHeartbeatPublisher>,
    counter: Option<EStopTopicCounter>,
    latch_path: PathBuf,
    forensic_dir: PathBuf,
}

impl EStopEnv {
    async fn bring_up(test_name: &str, cfg: CaseCfg) -> Self {
        let pid = std::process::id();
        let seq = CASE_SEQ.fetch_add(1, Ordering::Relaxed);
        let robot_id = format!("estop_e2e_{pid}_{seq}_{test_name}");
        let forensic_dir = PathBuf::from(format!("/tmp/estop_e2e/{robot_id}"));
        std::fs::create_dir_all(&forensic_dir).expect("create forensic dir");

        let listen_addr = alloc_loopback_port();

        // latch 预置（全部 case 从未 latch 起步；EStop 后断 "1"）。
        let latch_path = forensic_dir.join("estop_latch");
        std::fs::write(&latch_path, "0\n").expect("preset latch");

        let config_path = forensic_dir.join("robot.toml");
        std::fs::write(&config_path, build_config_toml(&robot_id, cfg.auto_enable))
            .expect("write config");

        let roles_path = forensic_dir.join("rpc_roles.toml");
        let roles_text =
            std::fs::read_to_string(workspace_rpc_roles_path()).expect("read workspace rpc_roles");
        std::fs::write(&roles_path, roles_text).expect("write rpc_roles");

        // ── 0) **PD1：counter 订阅者最先建**（先于 s0/gateway/第一次 RPC）──
        let counter = EStopTopicCounter::spawn(&robot_id);

        // ── 1) s0_daemon ──
        let s0_bin = locate_test_bin("s0_daemon").expect("locate s0_daemon bin");
        let mut s0 = SubprocessHandle::spawn(
            "s0",
            build_s0_command(&s0_bin, &robot_id, &config_path, &latch_path),
        )
        .expect("spawn s0_daemon");
        if let Err(e) = s0.wait_for_marker(S0_READY_MARKER, MARKER_TIMEOUT).await {
            for line in s0.snapshot_head_tail(30).await {
                eprintln!("[estop_e2e s0] {line}");
            }
            panic!("s0 ready marker timeout: {e}");
        }

        // ── 2) fake heartbeat（按需）──
        let heartbeat = if cfg.heartbeat {
            Some(FakeHeartbeatPublisher::spawn(&robot_id).expect("spawn FakeHeartbeatPublisher"))
        } else {
            None
        };

        // ── 3) gateway（PB1：marker 蕴含 estop publisher 已 attach）──
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
                eprintln!("[estop_e2e gateway] {line}");
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
            counter: Some(counter),
            latch_path,
            forensic_dir,
        };
        let _ = env.state_now().await;
        env
    }

    fn client(&self) -> RobotControlClient<Channel> {
        RobotControlClient::new(self.channel.clone())
    }

    /// 读当前 `RobotState`（吸收首帧前短暂 Unavailable）。
    async fn state_now(&self) -> (i32, i32) {
        let mut client = self.client();
        let deadline = tokio::time::Instant::now() + MODE_TIMEOUT;
        loop {
            match client
                .get_state(GetStateRequest {
                    hardware_id: HW.to_string(),
                })
                .await
            {
                Ok(resp) => {
                    let rs = resp.into_inner();
                    return (rs.mode, rs.safety_state);
                }
                Err(s)
                    if s.code() == Code::Unavailable && tokio::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(s) => panic!("get_state({HW}): {:?} {}", s.code(), s.message()),
            }
        }
    }

    /// 轮询 GetState 直到 `mode == want`。
    async fn wait_mode(&self, want: S0Mode) {
        let deadline = tokio::time::Instant::now() + MODE_TIMEOUT;
        loop {
            let (mode, _) = self.state_now().await;
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

    fn counter(&self) -> &EStopTopicCounter {
        self.counter.as_ref().expect("counter alive")
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
        if let Some(c) = self.counter.take() {
            c.shutdown();
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

fn build_config_toml(robot_id: &str, auto_enable: Option<bool>) -> String {
    use std::fmt::Write;
    let mut s = format!(
        "# v0.5 候选 D (2/3) estop_e2e 共用 config（s0 + gateway）\n\
         [robot]\nid = \"{robot_id}\"\nconfig_version = \"0.5.0\"\n\n\
         [[hardware]]\nid = \"{HW}\"\nadapter = \"mock_hard_rt\"\njoints = [\"j0\", \"j1\"]\nmotor_ids = [1, 2]\n"
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

fn make_estop_cmd(idempotency_key: &str, reason: &str, dry_run: bool, token: &str) -> EStopCommand {
    EStopCommand {
        meta: Some(CommandMeta {
            request_id: Uuid::now_v7().to_string(),
            source: "e2e.estop".into(),
            auth: Some(AuthContext {
                token: token.into(),
                mtls_fingerprint: String::new(),
            }),
            timeout_ms: 2_000,
            idempotency_key: idempotency_key.into(),
            dry_run,
            audit_reason: "estop_e2e".into(),
        }),
        estop_reason: reason.into(),
    }
}

fn make_set_enabled_cmd(idempotency_key: &str, token: &str) -> SetEnabledCommand {
    SetEnabledCommand {
        meta: Some(CommandMeta {
            request_id: Uuid::now_v7().to_string(),
            source: "e2e.estop".into(),
            auth: Some(AuthContext {
                token: token.into(),
                mtls_fingerprint: String::new(),
            }),
            timeout_ms: 2_000,
            idempotency_key: idempotency_key.into(),
            dry_run: false,
            audit_reason: "estop_e2e".into(),
        }),
        enabled: true,
    }
}

/// 断言运行态进入安全：mode=SAFE + safety_state ∈ {StopCommanded,
/// TorqueOffConfirmed}（EStop 后 rt_thread 异步推进，两值都合法）。
async fn assert_safe_and_stopped(env: &EStopEnv) {
    env.wait_mode(S0Mode::Safe).await;
    let deadline = tokio::time::Instant::now() + MODE_TIMEOUT;
    loop {
        let (_, safety) = env.state_now().await;
        if safety == S0SafetyState::StopCommanded as i32
            || safety == S0SafetyState::TorqueOffConfirmed as i32
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "safety_state 未进入 StopCommanded/TorqueOffConfirmed；最后观测 {safety}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ============================================================================
// AC-2 + AC-8 + 候选 C AC-4 闭环：Safe 态 EStop happy
// ============================================================================

#[tokio::test]
async fn estop_from_safe_happy() {
    let _guard = GLOBAL_LOCK.lock().await;
    let env = EStopEnv::bring_up(
        "safehappy",
        CaseCfg {
            auto_enable: Some(false), // 留 SAFE（happy 最小态）
            heartbeat: false,
        },
    )
    .await;
    let mut client = env.client();

    assert_eq!(env.latch_bytes().trim(), "0", "前置：latch 未置");

    let ack = client
        .e_stop(tonic::Request::new(make_estop_cmd(
            "idem-safehappy",
            "e2e safe estop",
            false,
            OPERATOR_TOKEN,
        )))
        .await
        .expect("EStop 应 Ack（published 弱语义）")
        .into_inner();
    assert!(ack.cleared_at_ns > 0, "published_at_ns 应非零");
    assert!(!ack.dry_run);

    // 运行态：SAFE + safety stop（harness 经 GetState 观测）。
    assert_safe_and_stopped(&env).await;

    // §6.7 #4：latch 文件 "1"（harness 本地读；fsync 由 s0 主循环 hook 完成）。
    let deadline = tokio::time::Instant::now() + MODE_TIMEOUT;
    while env.latch_bytes().trim() != "1" {
        assert!(
            tokio::time::Instant::now() < deadline,
            "latch 文件未在窗口内变 '1'；实际 {:?}",
            env.latch_bytes()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // AC-8：wire 字段（counter 订阅者记录）。
    tokio::time::sleep(COUNT_SETTLE).await;
    let records = env.counter().snapshot();
    assert_eq!(records.len(), 1, "应恰好 1 条 EStopEvent");
    let r = &records[0];
    assert_eq!(r.reason, "e2e safe estop", "reason 透传");
    assert_eq!(r.source, "rpc", "source 固定 rpc（D6）");
    assert!(!r.latched, "RPC 路径 latched=false（D3）");
    assert_eq!(r.envelope_source_id, 98, "gateway source_id=98（PD4）");

    // 候选 C AC-4 闭环：latch 后 enable 被拒 FailedPrecondition（LatchBlocked）。
    let err = client
        .set_enabled(tonic::Request::new(make_set_enabled_cmd(
            "idem-safehappy-en",
            OPERATOR_TOKEN,
        )))
        .await
        .expect_err("latched 后 enable 应拒");
    assert_eq!(err.code(), Code::FailedPrecondition);

    env.shutdown().await;
}

// ============================================================================
// AC-3：Enabled 态 EStop 压倒（Enabled→Safe Emergency）
// ============================================================================

#[tokio::test]
async fn estop_from_enabled_overrides() {
    let _guard = GLOBAL_LOCK.lock().await;
    let env = EStopEnv::bring_up(
        "enov",
        CaseCfg {
            auto_enable: None, // 缺省 true：auto-enable + heartbeat → ENABLED
            heartbeat: true,
        },
    )
    .await;
    let mut client = env.client();

    env.wait_mode(S0Mode::Enabled).await;
    assert_eq!(env.latch_bytes().trim(), "0");

    let ack = client
        .e_stop(tonic::Request::new(make_estop_cmd(
            "idem-enov",
            "e2e enabled estop",
            false,
            OPERATOR_TOKEN,
        )))
        .await
        .expect("Enabled 态 EStop 应 Ack")
        .into_inner();
    assert!(ack.cleared_at_ns > 0);

    // Enabled → Safe 压倒一切（heartbeat 还在跑也不回 Enabled——latch 挡住）。
    assert_safe_and_stopped(&env).await;
    let deadline = tokio::time::Instant::now() + MODE_TIMEOUT;
    while env.latch_bytes().trim() != "1" {
        assert!(tokio::time::Instant::now() < deadline, "latch 文件未变 '1'");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    env.shutdown().await;
}

// ============================================================================
// AC-4 / PB3：viewer 拒；operator / maintainer / admin 全准
// ============================================================================

#[tokio::test]
async fn viewer_denied_operator_maintainer_admin_allowed() {
    let _guard = GLOBAL_LOCK.lock().await;
    let env = EStopEnv::bring_up(
        "auth",
        CaseCfg {
            auto_enable: Some(false),
            heartbeat: false,
        },
    )
    .await;
    let mut client = env.client();

    let err = client
        .e_stop(tonic::Request::new(make_estop_cmd(
            "idem-auth-v",
            "viewer try",
            false,
            VIEWER_TOKEN,
        )))
        .await
        .expect_err("viewer 应 PermissionDenied");
    assert_eq!(err.code(), Code::PermissionDenied);

    // operator+ 全准（各自独立 key；EStop 幂等，多次触发无害）。
    for (key, token) in [
        ("idem-auth-op", OPERATOR_TOKEN),
        ("idem-auth-mt", MAINTAINER_TOKEN),
        ("idem-auth-ad", ADMIN_TOKEN),
    ] {
        let ack = client
            .e_stop(tonic::Request::new(make_estop_cmd(
                key,
                "authz probe",
                false,
                token,
            )))
            .await
            .unwrap_or_else(|e| panic!("{token} 应准，实际 {e:?}"))
            .into_inner();
        assert!(ack.cleared_at_ns > 0);
    }

    // viewer 被拒不产事件；3 个准的各 1 条 = 3 条。
    tokio::time::sleep(COUNT_SETTLE).await;
    assert_eq!(env.counter().snapshot().len(), 3, "3 个 operator+ 各 1 条");

    env.shutdown().await;
}

// ============================================================================
// AC-5 / PD1：同 key replay 单 publish；异 key 二次 publish
// ============================================================================

#[tokio::test]
async fn idem_replay_single_publish() {
    let _guard = GLOBAL_LOCK.lock().await;
    let env = EStopEnv::bring_up(
        "idem",
        CaseCfg {
            auto_enable: Some(false),
            heartbeat: false,
        },
    )
    .await;
    let mut client = env.client();

    // 同 key 两发（fingerprint 字段一致；request_id 变不进 fp）。
    let ack1 = client
        .e_stop(tonic::Request::new(make_estop_cmd(
            "idem-replay",
            "same reason",
            false,
            OPERATOR_TOKEN,
        )))
        .await
        .expect("第一发应 Ack")
        .into_inner();
    let ack2 = client
        .e_stop(tonic::Request::new(make_estop_cmd(
            "idem-replay",
            "same reason",
            false,
            OPERATOR_TOKEN,
        )))
        .await
        .expect("第二发应 Replay Ack")
        .into_inner();
    assert_eq!(
        ack1.cleared_at_ns, ack2.cleared_at_ns,
        "Replay 应返同 published_at_ns"
    );

    // PD1 判据：settle 后**恰好 1 条**（同 key 不重复 publish，§6.7 #5）。
    tokio::time::sleep(COUNT_SETTLE).await;
    assert_eq!(
        env.counter().snapshot().len(),
        1,
        "同 key 两发应恰好 1 条 EStopEvent"
    );

    // 异 key 再发 → 第二次 publish（latch 单调，重复触发合法）。
    let ack3 = client
        .e_stop(tonic::Request::new(make_estop_cmd(
            "idem-replay-2",
            "same reason",
            false,
            OPERATOR_TOKEN,
        )))
        .await
        .expect("异 key 应 Ack")
        .into_inner();
    assert!(ack3.cleared_at_ns >= ack1.cleared_at_ns);

    tokio::time::sleep(COUNT_SETTLE).await;
    assert_eq!(
        env.counter().snapshot().len(),
        2,
        "异 key 再发后应恰好 2 条"
    );

    env.shutdown().await;
}

// ============================================================================
// AC-6 / D4：dry_run=true → InvalidArgument，0 publish 0 副作用
// ============================================================================

#[tokio::test]
async fn dry_run_rejected() {
    let _guard = GLOBAL_LOCK.lock().await;
    let env = EStopEnv::bring_up(
        "dryrej",
        CaseCfg {
            auto_enable: Some(false),
            heartbeat: false,
        },
    )
    .await;
    let mut client = env.client();

    let err = client
        .e_stop(tonic::Request::new(make_estop_cmd(
            "idem-dryrej",
            "should not publish",
            true, // dry_run
            OPERATOR_TOKEN,
        )))
        .await
        .expect_err("dry_run 应 InvalidArgument（D4）");
    assert_eq!(err.code(), Code::InvalidArgument);
    assert!(
        err.message().contains("dry_run"),
        "错误信息应点名 dry_run；got: {}",
        err.message()
    );

    // 0 副作用：无事件 + mode 不变 + latch 不变。
    tokio::time::sleep(COUNT_SETTLE).await;
    assert_eq!(env.counter().snapshot().len(), 0, "dry_run 不得 publish");
    let (mode, _) = env.state_now().await;
    assert_eq!(mode, S0Mode::Safe as i32);
    assert_eq!(env.latch_bytes().trim(), "0", "dry_run 不得碰 latch");

    env.shutdown().await;
}

// ============================================================================
// AC-7 / PD2 / PB1：S0 不在 → 仍 Ack（published 弱语义）
// ============================================================================

#[tokio::test]
async fn s0_dead_still_acks() {
    let _guard = GLOBAL_LOCK.lock().await;
    let mut env = EStopEnv::bring_up(
        "s0dead",
        CaseCfg {
            auto_enable: Some(false),
            heartbeat: false,
        },
    )
    .await;
    let mut client = env.client();

    // **顺序硬性（PB1）**：gateway ready marker（蕴含 estop publisher 已
    // attach）已在 bring_up 完成 → 现在退场 s0。
    let s0 = env.s0.take().expect("s0 alive");
    let status = s0.shutdown(SHUTDOWN_GRACE).await.expect("s0 shutdown");
    assert!(status.success(), "s0 退场应 clean");

    // S0 不在 → EStop 仍 Ack（publish 不依赖订阅者存活；D2 弱语义 + §6.7 #1
    // 该场景由硬件 fail-safe 兜底——RPC 不是救命索）。
    let ack = client
        .e_stop(tonic::Request::new(make_estop_cmd(
            "idem-s0dead",
            "estop with dead s0",
            false,
            OPERATOR_TOKEN,
        )))
        .await
        .expect("S0 不在仍应 Ack（published 语义）")
        .into_inner();
    assert!(ack.cleared_at_ns > 0);

    // 事件确实发布了（counter 独立于 s0 存活）。
    tokio::time::sleep(COUNT_SETTLE).await;
    assert_eq!(
        env.counter().snapshot().len(),
        1,
        "S0 不在事件仍应发布（订阅者独立收到）"
    );

    env.shutdown().await;
}
