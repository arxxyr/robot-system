//! `CaseCtx`：单 test fn 一份，统管 robot_id / forensic dir / s1_daemon /
//! gateway / fake state 子进程的整个 lifecycle。
//!
//! ## 顺序：bring_up
//!
//! 1. 派生唯一 `robot_id` (`task_rpc_e2e_<pid>_<seq>`)
//! 2. 分配 loopback `listen_addr` (TcpListener::bind 0 + drop)
//! 3. 创建 forensic dir `/tmp/b4_e2e/<robot_id>/`
//! 4. 写 s1.toml + 复制 rpc_roles.toml 到 forensic dir
//! 5. spawn `FakeStatePublisher` (在 spawn s1 **之前**，保证 anchor 可用)
//! 6. 找 `s1_daemon` / `robot_rpc_gateway` 二进制路径
//! 7. spawn s1_daemon
//! 8. spawn gateway
//! 9. wait_ready 三段式 → 拿 tonic Channel
//!
//! ## 顺序：shutdown
//!
//! 1. gateway.shutdown(3s grace)
//! 2. daemon.shutdown(3s grace)
//! 3. fake_state.shutdown()
//! 4. 任一子进程 exit 非 0 → forensic log + Err
//!
//! ## Drop 兜底
//!
//! `kill_on_drop(true)` 已在 `SubprocessHandle::spawn` 设置；test panic
//! 时 child SIGKILL；本 Drop 仅 forensic log（不阻塞 tokio worker）。

use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use thiserror::Error;
use tonic::transport::Channel;

use crate::common::fake_state::FakeStatePublisher;
use crate::common::ready::{ReadyError, wait_ready};
use crate::common::subprocess::{
    ShutdownError, SubprocessHandle, build_gateway_command, build_s1_daemon_command,
    locate_test_bin,
};

const HARDWARE_ID: &str = "test_arm";
const JOINTS_COUNT: usize = 1;
const LAYOUT_ID: u64 = 0xCAFE_BABE_DEAD_BEEF;

/// `CASE_SEQ` 给每个 test fn 派生唯一 `robot_id`，避免 iceoryx2 service
/// 名 / loopback port 在并发 spawn 时撞车（即使 `--test-threads=1` 串行
/// 跑，跨 cargo test 重跑也靠 `pid + seq` 隔离避免残留）。
static CASE_SEQ: AtomicU32 = AtomicU32::new(0);

const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

#[derive(Debug, Error)]
pub enum BringUpError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("ready: {0}")]
    Ready(#[from] ReadyError),
    #[error("locate bin: {0}")]
    LocateBin(String),
    #[error("config: {0}")]
    Config(String),
    #[error("port allocation: {0}")]
    Port(String),
    #[error("fake state: {0}")]
    FakeState(String),
}

#[derive(Debug, Error)]
pub enum CaseShutdownError {
    #[error("daemon shutdown: {0}")]
    Daemon(ShutdownError),
    #[error("gateway shutdown: {0}")]
    Gateway(ShutdownError),
    #[error("daemon exit !=0: {status:?}")]
    DaemonExitNonZero { status: std::process::ExitStatus },
    #[error("gateway exit !=0: {status:?}")]
    GatewayExitNonZero { status: std::process::ExitStatus },
}

pub struct CaseCtx {
    pub robot_id: String,
    pub hardware_id: &'static str,
    pub layout_id: u64,
    pub joints: usize,
    pub listen_addr: SocketAddr,
    pub config_path: PathBuf,
    pub roles_toml_path: PathBuf,
    pub forensic_dir: PathBuf,
    /// 拿到的 tonic Channel；caller 用 `RobotControlClient::new(channel)` 构造 client。
    pub channel: Channel,

    daemon: Option<SubprocessHandle>,
    gateway: Option<SubprocessHandle>,
    fake_state: Option<FakeStatePublisher>,
}

impl CaseCtx {
    /// 主入口：spawn fake state → s1_daemon → gateway → wait_ready。
    ///
    /// # Errors
    ///
    /// 见 [`BringUpError`]。
    pub async fn bring_up(test_name: &'static str) -> Result<Self, BringUpError> {
        let pid = std::process::id();
        let seq = CASE_SEQ.fetch_add(1, Ordering::Relaxed);
        let robot_id = format!("task_rpc_e2e_{pid}_{seq}_{test_name}");

        let forensic_dir = PathBuf::from(format!("/tmp/b4_e2e/{robot_id}"));
        tokio::fs::create_dir_all(&forensic_dir)
            .await
            .map_err(|e| {
                BringUpError::Config(format!(
                    "create forensic dir {}: {e}",
                    forensic_dir.display()
                ))
            })?;

        let listen_addr = allocate_loopback_port()?;
        let config_path = forensic_dir.join("s1.toml");
        let s1_toml = build_s1_toml(&robot_id, HARDWARE_ID, &["test_j0".to_string()]);
        tokio::fs::write(&config_path, s1_toml)
            .await
            .map_err(|e| BringUpError::Config(format!("write s1.toml: {e}")))?;

        let roles_toml_path = forensic_dir.join("rpc_roles.toml");
        let roles_toml = std::fs::read_to_string(workspace_rpc_roles_path()?)
            .map_err(|e| BringUpError::Config(format!("read workspace rpc_roles.toml: {e}")))?;
        tokio::fs::write(&roles_toml_path, &roles_toml)
            .await
            .map_err(|e| BringUpError::Config(format!("write rpc_roles.toml: {e}")))?;

        // 1. spawn fake state（**先于** s1，保 anchor 5s 内拿到 stable bias）
        let fake_state = FakeStatePublisher::spawn(&robot_id, HARDWARE_ID, JOINTS_COUNT, LAYOUT_ID)
            .map_err(|e| BringUpError::FakeState(e.to_string()))?;

        // 2. locate s1_daemon binary
        let s1_bin = locate_test_bin("s1_daemon").map_err(BringUpError::LocateBin)?;
        let mut daemon = SubprocessHandle::spawn(
            "s1_daemon",
            build_s1_daemon_command(&s1_bin, &robot_id, HARDWARE_ID, &config_path),
        )?;

        // 3. locate gateway binary
        let gateway_bin = locate_test_bin("robot_rpc_gateway").map_err(BringUpError::LocateBin)?;
        let mut gateway = SubprocessHandle::spawn(
            "gateway",
            build_gateway_command(&gateway_bin, &robot_id, listen_addr, &roles_toml_path),
        )?;

        // 4. wait_ready 三段式
        let channel = match wait_ready(&mut daemon, &mut gateway, listen_addr).await {
            Ok(c) => c,
            Err(e) => {
                // bring_up 失败：dump forensic 让 dev 立刻看到 head/tail
                let head_tail_daemon = daemon.snapshot_head_tail(20).await;
                let head_tail_gateway = gateway.snapshot_head_tail(20).await;
                eprintln!(
                    "[FORENSIC bring_up] robot_id={robot_id}, listen_addr={listen_addr}, \
                     daemon_pid={:?}, gateway_pid={:?}",
                    daemon.pid(),
                    gateway.pid()
                );
                eprintln!("[FORENSIC bring_up daemon head/tail]:");
                for line in head_tail_daemon {
                    eprintln!("  {line}");
                }
                eprintln!("[FORENSIC bring_up gateway head/tail]:");
                for line in head_tail_gateway {
                    eprintln!("  {line}");
                }
                let _ = daemon
                    .dump_captured_to(&forensic_dir.join("daemon.stderr"))
                    .await;
                let _ = gateway
                    .dump_captured_to(&forensic_dir.join("gateway.stderr"))
                    .await;
                return Err(BringUpError::Ready(e));
            }
        };

        Ok(Self {
            robot_id,
            hardware_id: HARDWARE_ID,
            layout_id: LAYOUT_ID,
            joints: JOINTS_COUNT,
            listen_addr,
            config_path,
            roles_toml_path,
            forensic_dir,
            channel,
            daemon: Some(daemon),
            gateway: Some(gateway),
            fake_state: Some(fake_state),
        })
    }

    /// graceful shutdown：gateway → daemon → fake_state；任一子进程 exit
    /// !=0 返 Err（test fn `unwrap` 即 PASS/FAIL 判定）。
    ///
    /// # Errors
    ///
    /// 见 [`CaseShutdownError`]。
    pub async fn shutdown(mut self) -> Result<(), CaseShutdownError> {
        // 1. gateway 先停（client 看不到新 RPC；rr_io_thread 在 tonic exit 后停）
        let gw_status = if let Some(gw) = self.gateway.take() {
            // dump forensic 之前先 shutdown
            // (注：shutdown 会消费 self；先 dump 不行)
            gw.shutdown(SHUTDOWN_GRACE).await
        } else {
            // unreachable：bring_up 成功后 gateway 必 Some
            return Ok(());
        };

        // 2. daemon 停
        let daemon_status = if let Some(daemon) = self.daemon.take() {
            daemon.shutdown(SHUTDOWN_GRACE).await
        } else {
            return Ok(());
        };

        // 3. fake_state 停（必须在 daemon 之后，否则 daemon 会在 tick loop
        //    退出时尝试 anchor read 失败 log noise）
        if let Some(fs) = self.fake_state.take() {
            fs.shutdown();
        }

        // 4. 验 exit code 非 0 = test 失败信号
        let gw_status = gw_status.map_err(CaseShutdownError::Gateway)?;
        if !gw_status.success() {
            return Err(CaseShutdownError::GatewayExitNonZero { status: gw_status });
        }
        let daemon_status = daemon_status.map_err(CaseShutdownError::Daemon)?;
        if !daemon_status.success() {
            return Err(CaseShutdownError::DaemonExitNonZero {
                status: daemon_status,
            });
        }

        Ok(())
    }
}

impl Drop for CaseCtx {
    fn drop(&mut self) {
        // RAII 兜底：kill_on_drop=true 已让 child SIGKILL；这里仅 log。
        // shutdown() 显式调过会先 take Option 字段，drop 时 None 跳过。
        if let Some(daemon) = &self.daemon {
            eprintln!(
                "[FORENSIC drop] daemon still active: pid={:?}, robot_id={}, \
                 listen_addr={}, kill_on_drop=true",
                daemon.pid(),
                self.robot_id,
                self.listen_addr,
            );
        }
        if let Some(gw) = &self.gateway {
            eprintln!(
                "[FORENSIC drop] gateway still active: pid={:?}, robot_id={}, \
                 listen_addr={}, kill_on_drop=true",
                gw.pid(),
                self.robot_id,
                self.listen_addr,
            );
        }
        // fake_state 的 Drop 自身会 join thread；这里不动。
    }
}

// ============================================================================
// Helpers
// ============================================================================

fn allocate_loopback_port() -> Result<SocketAddr, BringUpError> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|e| BringUpError::Port(format!("TcpListener::bind 0: {e}")))?;
    let addr = listener
        .local_addr()
        .map_err(|e| BringUpError::Port(format!("local_addr: {e}")))?;
    drop(listener);
    Ok(addr)
}

fn build_s1_toml(robot_id: &str, hardware_id: &str, joints: &[String]) -> String {
    let joints_csv = joints
        .iter()
        .map(|j| format!("\"{j}\""))
        .collect::<Vec<_>>()
        .join(", ");
    // **不**写 [s1] 段：parse_s1_section_from_toml 含 deny_unknown_fields，
    // [s1] + [s1.profile] 共存会被 serde 当 [s1] 子字段 profile 拒（详
    // s1/coordinator.rs::S1Section）。**全 default**：CoordinatorConfig::default
    // + ProfileSection::default_b2（mock_dwell + joint_position），跟测试
    // 路径完全等价（mock_dwell 100ms 终态行为不变）。
    format!(
        r#"# B4 (3/3) cross-process e2e test config
# Generated by tests/common/case_ctx.rs::build_s1_toml
[robot]
id             = "{robot_id}"
config_version = "0.3.0"

[[hardware]]
id      = "{hardware_id}"
adapter = "mock_hard_rt"
joints  = [{joints_csv}]
"#
    )
}

/// 找 workspace 的 `configs/rpc_roles.toml`。从 CARGO_MANIFEST_DIR 派生。
fn workspace_rpc_roles_path() -> Result<PathBuf, BringUpError> {
    // CARGO_MANIFEST_DIR = .../crates/robot-rpc → ../../configs/rpc_roles.toml
    let manifest = env!("CARGO_MANIFEST_DIR");
    let path = PathBuf::from(manifest)
        .parent()
        .and_then(|p| p.parent())
        .ok_or_else(|| {
            BringUpError::Config(format!("CARGO_MANIFEST_DIR={manifest:?} no grandparent"))
        })?
        .join("configs/rpc_roles.toml");
    if !path.exists() {
        return Err(BringUpError::Config(format!(
            "{} 不存在（workspace 结构改了？）",
            path.display()
        )));
    }
    Ok(path)
}
