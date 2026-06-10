//! v0.5 候选 B (4/4) `GetState` / `StreamState` cross-process e2e。
//!
//! ## 拓扑（gateway-only，**不**起 s1）
//!
//! state RPC 读 gateway 自己的常驻 state 池（D4），池订阅
//! `/<robot_id>/s0/<hardware_id>/state` 上 [`FakeStatePublisher`] 喂的
//! `JointState`——**不经 s1**（s1 是 coordinator，不持权威硬件 state）。故本
//! e2e 只起 **gateway（带 `--config`）+ N 个 FakeStatePublisher**，比
//! `task_rpc_e2e` 更轻。
//!
//! ## 串行
//!
//! 同 `task_rpc_e2e`：`GLOBAL_LOCK` + 建议 `--test-threads=1`。每 test fn 派生
//! 唯一 `robot_id`（`state_rpc_e2e_<pid>_<seq>_<name>`）+ 独立 loopback port
//! （D-P3 进程隔离，避免与 task_rpc_e2e / 残留 iceoryx2 service 互扰）。
//!
//! ## Feature gate
//!
//! `RobotControlClient` 需 `client` feature；`[[test]] required-features =
//! ["client"]` 守门，workspace feature unification 下默认随 `cargo test
//! --workspace` 跑（A4）。
//!
//! ## 覆盖
//!
//! | AC | test fn |
//! |----|---------|
//! | AC-2 + AC-7 | `get_state_happy_field_equivalence_and_stamp_passthrough` |
//! | AC-3 | `get_state_error_codes_invalid_unknown_unavailable` |
//! | AC-9 | `get_state_undeclared_hardware_notfound_no_fallback` |
//! | AC-6 | `read_rpc_auth_token_does_not_affect_result`（GetState + StreamState）|
//! | AC-4 | `stream_state_throttle_50ms_window_600ms` |
//! | AC-5 | `stream_state_clamp_below_floor_to_20ms` |

#![cfg(target_os = "linux")]
// 测试 doc / 断言里大量 identifier-like 字串（GetState / StreamState / Instant /
// Duration / NotFound 等）走宽松 doc-markdown（同 tests/common/mod.rs）。
#![allow(clippy::doc_markdown)]

mod common;

use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tonic::Code;
use tonic::transport::{Channel, Endpoint};

use common::fake_state::FakeStatePublisher;
use common::subprocess::{SubprocessHandle, build_gateway_command, locate_test_bin};
use robot_rpc::proto::robot_control_client::RobotControlClient;
use robot_rpc::proto::{GetStateRequest, RobotState, StreamRequest};

/// 强制 6 个 `#[tokio::test]` 串行（双保险：跟 `--test-threads=1` 并存）。
/// **必须** `tokio::sync::Mutex`（guard 跨 `.await` 安全）。
static GLOBAL_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

static CASE_SEQ: AtomicU32 = AtomicU32::new(0);

const LAYOUT_ID: u64 = 0x0BAD_F00D_1234_5678;
/// AC-7：固定 stamp 让 GetState 确定性断言 stamp 透传（非 gateway 自生 clock）。
const FIXED_STAMP_NS: u64 = 1_700_000_000_000;
const JOINTS: usize = 2;
// count-independent 前缀（v0.5 候选 C 修，同 common/ready.rs：计数随 method
// append 变化，全串硬编码会 timeout 假死）。
const GATEWAY_READY_MARKER: &str = "rr_io_threads started";
const MARKER_TIMEOUT: Duration = Duration::from_secs(5);
const READY_TIMEOUT: Duration = Duration::from_secs(5);
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(3);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);
/// readiness / AC-9 用：永不出现在任何 `--config` 的 hardware id。
const NEVER_HARDWARE: &str = "never_declared_hw";

// ============================================================================
// gateway-only e2e 环境
// ============================================================================

struct StateEnv {
    robot_id: String,
    channel: Channel,
    gateway: Option<SubprocessHandle>,
    publishers: Vec<FakeStatePublisher>,
    forensic_dir: PathBuf,
}

impl StateEnv {
    /// `declared` = gateway `--config` 的 `[[hardware]]` 全集；`published` =
    /// 其中起 `FakeStatePublisher`（固定 stamp）的子集。
    async fn bring_up(test_name: &str, declared: &[&str], published: &[&str]) -> Self {
        let pid = std::process::id();
        let seq = CASE_SEQ.fetch_add(1, Ordering::Relaxed);
        let robot_id = format!("state_rpc_e2e_{pid}_{seq}_{test_name}");
        let forensic_dir = PathBuf::from(format!("/tmp/state_e2e/{robot_id}"));
        std::fs::create_dir_all(&forensic_dir).expect("create forensic dir");

        let listen_addr = alloc_loopback_port();

        // gateway --config（[robot].id 必须 == robot_id；[[hardware]] per declared）。
        let config_path = forensic_dir.join("robot.toml");
        std::fs::write(&config_path, build_config_toml(&robot_id, declared)).expect("write config");

        // rpc_roles.toml（gateway 必填；复用 workspace configs/rpc_roles.toml）。
        let roles_path = forensic_dir.join("rpc_roles.toml");
        let roles_text =
            std::fs::read_to_string(workspace_rpc_roles_path()).expect("read workspace rpc_roles");
        std::fs::write(&roles_path, roles_text).expect("write rpc_roles");

        // FakeStatePublisher per published hardware（固定 stamp，AC-7 确定性）。
        let publishers = published
            .iter()
            .map(|hw| {
                FakeStatePublisher::spawn(&robot_id, hw, JOINTS, LAYOUT_ID, Some(FIXED_STAMP_NS))
                    .expect("spawn FakeStatePublisher")
            })
            .collect();

        // gateway（带 --config）。
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

        // readiness：gateway marker → connect → GetState(NEVER) == NotFound
        // （证 gateway state plane 已 serve，不依赖 publisher 首帧时机）。
        if let Err(e) = gateway
            .wait_for_marker(GATEWAY_READY_MARKER, MARKER_TIMEOUT)
            .await
        {
            let tail = gateway.snapshot_head_tail(30).await;
            for line in tail {
                eprintln!("[state_e2e gateway] {line}");
            }
            panic!("gateway ready marker timeout: {e}");
        }
        let channel = connect_and_probe(listen_addr).await;

        Self {
            robot_id,
            channel,
            gateway: Some(gateway),
            publishers,
            forensic_dir,
        }
    }

    fn client(&self) -> RobotControlClient<Channel> {
        RobotControlClient::new(self.channel.clone())
    }

    async fn shutdown(mut self) {
        if let Some(gw) = self.gateway.take() {
            let status = gw.shutdown(SHUTDOWN_GRACE).await.expect("gateway shutdown");
            assert!(status.success(), "gateway exit != 0: {status:?}");
        }
        for p in self.publishers.drain(..) {
            p.shutdown();
        }
        // 成功路径清理 forensic dir（失败路径 panic 已留现场）。
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

fn build_config_toml(robot_id: &str, declared: &[&str]) -> String {
    use std::fmt::Write;
    let mut s = format!(
        "# v0.5 候选 B (4/4) state_rpc_e2e gateway --config\n\
         [robot]\nid = \"{robot_id}\"\nconfig_version = \"0.3.0\"\n\n"
    );
    for hw in declared {
        let _ = write!(
            s,
            "[[hardware]]\nid = \"{hw}\"\nadapter = \"mock_hard_rt\"\njoints = [\"j0\", \"j1\"]\n\n"
        );
    }
    s
}

fn workspace_rpc_roles_path() -> PathBuf {
    // CARGO_MANIFEST_DIR = .../crates/robot-rpc → ../../configs/rpc_roles.toml
    let manifest = env!("CARGO_MANIFEST_DIR");
    let path = PathBuf::from(manifest)
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .join("configs/rpc_roles.toml");
    assert!(path.exists(), "{} 不存在", path.display());
    path
}

/// connect tonic Channel + GetState(NEVER) → NotFound readiness probe。
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

/// 重试 GetState 直到 Ok（吸收"池已建但首帧未到"的短暂 Unavailable）。
async fn get_state_until_ok(
    client: &mut RobotControlClient<Channel>,
    hardware_id: &str,
) -> RobotState {
    let deadline = tokio::time::Instant::now() + FIRST_FRAME_TIMEOUT;
    loop {
        match client
            .get_state(GetStateRequest {
                hardware_id: hardware_id.to_string(),
            })
            .await
        {
            Ok(resp) => return resp.into_inner(),
            Err(s) if s.code() == Code::Unavailable && tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(s) => panic!("get_state({hardware_id}): {:?} {}", s.code(), s.message()),
        }
    }
}

/// 取 StreamState 首帧（有界等待，防挂）。
async fn first_stream_frame(mut stream: tonic::Streaming<RobotState>) -> RobotState {
    match tokio::time::timeout(FIRST_FRAME_TIMEOUT, stream.message()).await {
        Ok(Ok(Some(rs))) => rs,
        other => {
            panic!("expected first StreamState frame within {FIRST_FRAME_TIMEOUT:?}: {other:?}")
        }
    }
}

/// 收 StreamState response 在 `window` 内的逐帧到达时刻（单调 `Instant`）。
async fn collect_stream_arrivals(
    mut stream: tonic::Streaming<RobotState>,
    window: Duration,
) -> Vec<tokio::time::Instant> {
    let start = tokio::time::Instant::now();
    let mut arrivals = Vec::new();
    loop {
        let elapsed = start.elapsed();
        if elapsed >= window {
            break;
        }
        let remaining = window.saturating_sub(elapsed);
        match tokio::time::timeout(remaining, stream.message()).await {
            Ok(Ok(Some(_rs))) => arrivals.push(tokio::time::Instant::now()),
            Ok(Err(status)) => panic!("StreamState frame error: {status:?}"),
            // 流结束 / window 到 → 停。
            Ok(Ok(None)) | Err(_) => break,
        }
    }
    arrivals
}

/// 节流判据（D-P2）：窗口内帧数 ≤ `max_frames` 且相邻帧间隔 ≥ `min_gap`。
fn assert_throttle(arrivals: &[tokio::time::Instant], max_frames: usize, min_gap: Duration) {
    assert!(
        !arrivals.is_empty(),
        "StreamState 窗口内 0 帧（期望首 tick 立即）"
    );
    assert!(
        arrivals.len() <= max_frames,
        "帧数 {} > 上限 {max_frames}",
        arrivals.len()
    );
    for w in arrivals.windows(2) {
        let gap = w[1] - w[0];
        assert!(
            gap >= min_gap,
            "相邻帧间隔 {gap:?} < 下限 {min_gap:?}（节流被破坏）"
        );
    }
}

// ============================================================================
// AC-2 + AC-7：GetState happy + 字段等价 + stamp 透传
// ============================================================================

#[tokio::test]
#[allow(clippy::float_cmp)] // position_rad 是精确发布的 0.0 字面值
async fn get_state_happy_field_equivalence_and_stamp_passthrough() {
    let _guard = GLOBAL_LOCK.lock().await;
    let env = StateEnv::bring_up("happy", &["arm"], &["arm"]).await;
    let mut client = env.client();

    let rs = get_state_until_ok(&mut client, "arm").await;

    assert_eq!(rs.robot_id, env.robot_id);
    assert_eq!(rs.hardware_id, "arm");
    assert_eq!(rs.layout_id, LAYOUT_ID);
    // AC-7：stamp 透传 S0 published 固定值（非 gateway 自生 clock）。
    assert_eq!(rs.stamp_ns, FIXED_STAMP_NS);
    assert_eq!(rs.position_rad.len(), JOINTS);
    assert!(rs.position_rad.iter().all(|p| *p == 0.0));
    // FakeStatePublisher 不写 s0_* → 默认 → proto *_UNSPECIFIED / 零值。
    assert_eq!(rs.mode, 0);
    assert_eq!(rs.control_link, 0);
    assert_eq!(rs.output_mode, 0);
    assert_eq!(rs.safety_state, 0);
    assert_eq!(rs.fault_code, 0);

    env.shutdown().await;
}

// ============================================================================
// AC-3：GetState 错误码三态（缺/空 InvalidArgument / unknown NotFound /
//        已知但无首帧 Unavailable）
// ============================================================================

#[tokio::test]
async fn get_state_error_codes_invalid_unknown_unavailable() {
    let _guard = GLOBAL_LOCK.lock().await;
    // declared arm + ghost；只 publish arm → ghost 永远无首帧。
    let env = StateEnv::bring_up("errcodes", &["arm", "ghost"], &["arm"]).await;
    let mut client = env.client();

    // 等 arm 首帧就绪，确保 pool 已 ready（排除"池没建"干扰）。
    let _ = get_state_until_ok(&mut client, "arm").await;

    // ① 空 hardware_id → InvalidArgument。
    let err = client
        .get_state(GetStateRequest {
            hardware_id: String::new(),
        })
        .await
        .expect_err("empty hardware_id must error");
    assert_eq!(err.code(), Code::InvalidArgument);

    // ② 未声明 hardware → NotFound。
    let err = client
        .get_state(GetStateRequest {
            hardware_id: "undeclared".to_string(),
        })
        .await
        .expect_err("undeclared hardware_id must error");
    assert_eq!(err.code(), Code::NotFound);

    // ③ 已声明但无 publisher（无首帧）→ Unavailable。
    let err = client
        .get_state(GetStateRequest {
            hardware_id: "ghost".to_string(),
        })
        .await
        .expect_err("declared-but-unpublished must error");
    assert_eq!(err.code(), Code::Unavailable);

    env.shutdown().await;
}

// ============================================================================
// AC-9：多 hardware，请求未声明的 → NotFound，禁默默 fallback
// ============================================================================

#[tokio::test]
async fn get_state_undeclared_hardware_notfound_no_fallback() {
    let _guard = GLOBAL_LOCK.lock().await;
    // 2 hardware config，两者都 publish。
    let env = StateEnv::bring_up("ac9", &["arm", "base"], &["arm", "base"]).await;
    let mut client = env.client();

    // 两个声明的都能拿到（证非 fallback 到单一）。
    let arm = get_state_until_ok(&mut client, "arm").await;
    assert_eq!(arm.hardware_id, "arm");
    let base = get_state_until_ok(&mut client, "base").await;
    assert_eq!(base.hardware_id, "base");

    // 请求第 3 个未声明的 → NotFound（**禁** fallback 到 arm/base）。
    let err = client
        .get_state(GetStateRequest {
            hardware_id: "phantom".to_string(),
        })
        .await
        .expect_err("3rd undeclared hardware must NotFound");
    assert_eq!(err.code(), Code::NotFound);

    env.shutdown().await;
}

// ============================================================================
// AC-6：无 token / 任意 metadata token 结果完全一致（read RPC 不鉴权，非安全
//        保证）—— GetState **和** StreamState 都覆盖
// ============================================================================

#[tokio::test]
async fn read_rpc_auth_token_does_not_affect_result() {
    let _guard = GLOBAL_LOCK.lock().await;
    let env = StateEnv::bring_up("auth", &["arm"], &["arm"]).await;
    let mut client = env.client();

    // 等首帧。
    let gs_no_token = get_state_until_ok(&mut client, "arm").await;

    // ── GetState：无 token vs 带任意 metadata token（gateway 不读不校验）──
    let mut gs_req = tonic::Request::new(GetStateRequest {
        hardware_id: "arm".to_string(),
    });
    gs_req.metadata_mut().insert(
        "authorization",
        "Bearer garbage-token-not-validated".parse().unwrap(),
    );
    let gs_with_token = client
        .get_state(gs_req)
        .await
        .expect("GetState with metadata token must still succeed")
        .into_inner();
    assert_eq!(gs_no_token, gs_with_token, "GetState token 影响了结果");

    // ── StreamState：无 token vs 带任意 metadata token，各取首帧逐字段相等 ──
    let ss_no_stream = client
        .stream_state(StreamRequest {
            hardware_id: "arm".to_string(),
            min_interval_ms: 20,
        })
        .await
        .expect("StreamState (no token) open")
        .into_inner();
    let ss_no_token = first_stream_frame(ss_no_stream).await;

    let mut ss_req = tonic::Request::new(StreamRequest {
        hardware_id: "arm".to_string(),
        min_interval_ms: 20,
    });
    ss_req.metadata_mut().insert(
        "authorization",
        "Bearer garbage-token-not-validated".parse().unwrap(),
    );
    let ss_with_stream = client
        .stream_state(ss_req)
        .await
        .expect("StreamState with metadata token must still succeed")
        .into_inner();
    let ss_with_token = first_stream_frame(ss_with_stream).await;
    assert_eq!(ss_no_token, ss_with_token, "StreamState token 影响了结果");

    // GetState 与 StreamState 首帧同源（同一 fixed-stamp 帧）→ 逐字段相等。
    assert_eq!(gs_no_token, ss_no_token);

    env.shutdown().await;
}

// ============================================================================
// AC-4：StreamState 节流 50ms（D-P2：window 600ms 帧 ≤13 + 相邻 ≥40ms）
// ============================================================================

#[tokio::test]
async fn stream_state_throttle_50ms_window_600ms() {
    let _guard = GLOBAL_LOCK.lock().await;
    let env = StateEnv::bring_up("thr50", &["arm"], &["arm"]).await;
    let mut client = env.client();
    // 确保首帧已进 pool（StreamState 开流要求有帧）。
    let _ = get_state_until_ok(&mut client, "arm").await;

    let stream = client
        .stream_state(StreamRequest {
            hardware_id: "arm".to_string(),
            min_interval_ms: 50,
        })
        .await
        .expect("stream_state open")
        .into_inner();

    let arrivals = collect_stream_arrivals(stream, Duration::from_millis(600)).await;
    // D-P2：interval=50 → window=600ms 帧 ≤ ⌈600/50⌉+1 = 13，相邻 ≥ 40ms。
    assert_throttle(&arrivals, 13, Duration::from_millis(40));

    env.shutdown().await;
}

// ============================================================================
// AC-5：min_interval_ms < floor 20 → clamp 到 20ms（D-P2：window 240ms 帧
//        ≤13 + 相邻 ≥16ms）
// ============================================================================

#[tokio::test]
async fn stream_state_clamp_below_floor_to_20ms() {
    let _guard = GLOBAL_LOCK.lock().await;
    let env = StateEnv::bring_up("clamp", &["arm"], &["arm"]).await;
    let mut client = env.client();
    let _ = get_state_until_ok(&mut client, "arm").await;

    // min_interval_ms = 5 (< floor 20) → clamp 20ms。
    let stream = client
        .stream_state(StreamRequest {
            hardware_id: "arm".to_string(),
            min_interval_ms: 5,
        })
        .await
        .expect("stream_state open")
        .into_inner();

    let arrivals = collect_stream_arrivals(stream, Duration::from_millis(240)).await;
    // D-P2 clamp 路径：interval=20 → window=240ms 帧 ≤ ⌈240/20⌉+1 = 13，相邻 ≥ 16ms。
    assert_throttle(&arrivals, 13, Duration::from_millis(16));

    env.shutdown().await;
}
