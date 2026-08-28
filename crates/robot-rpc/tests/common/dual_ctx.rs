//! `DualCtx`：**2×(s0 + s1 + gateway) = 6 子进程**双臂 e2e harness
//! （v0.5 候选 E 段(7)，Spec PD-E3 + D4 + D9 "三真同堂首例"）。
//!
//! ## 跟既有 `CaseCtx`（B4，`task_rpc_e2e.rs`）的差别
//!
//! | 项 | `CaseCtx` | `DualCtx` |
//! |---|---|---|
//! | 拓扑 | FakeState + s1 + gateway（3 子进程/线程） | **真 s0** + s1 + gateway **×2 臂** |
//! | state 源 | `FakeStatePublisher`（测试线程） | **真 s0_daemon**（mock_hard_rt adapter） |
//! | robot_id | `task_rpc_e2e_<pid>_<seq>_<test>` | `dimos_e2e_<pid>_<seq>_{left,right}` |
//! | hardware | `test_arm` | 固定 `left_arm` / `right_arm`（PD-E3） |
//! | 端口 | 1 个 | **2 个**，均由 `127.0.0.1:0` 分配并**断言不同** |
//! | 并发 | 无 | 单 test `GLOBAL_LOCK` 串行（6 进程 × 并发 = 资源打架） |
//!
//! ## bring-up 顺序（每臂）
//!
//! 1. provision latch primary + bak = `0\n`（s0 strict fail-closed，
//!    `scripts/systemd/README.md` "首次部署" 同款语义）；
//! 2. 写 per-case config（`[robot].id == --robot-id` 身份锁，段(2)）；
//! 3. spawn **s0_daemon** → 等 marker `set_enabled_rr_server started`
//!    （s0 全部线程 + 两个 RR server 就绪；早于它 spawn s1 会让 s1 锚定
//!    读不到 state → exit 4）；
//! 4. spawn **s1_daemon**（启动锚定 + 段(5) state cache 订阅 s0 state）；
//! 5. spawn **gateway**（带 `--config`，state 池订阅 s0 state 供 `GetState`）；
//! 6. `wait_ready` 三段式（s1 marker → gateway marker → TCP → RPC probe）。
//!
//! ## 关节谱（[`JointsSpec`]）
//!
//! - [`JointsSpec::ArmWithGripper`]（默认）：D2(i) 的 **6 维**臂向量
//!   （5 臂关节 + gripper 第 6 位），名字沿权威 config 的
//!   `<side>_arm_*` 形态；
//! - [`JointsSpec::IkSolver5`]：**5 维**，名字 == `XLeRobotArmIkSolver`
//!   的 `info().joint_names`。
//!
//! **为什么需要第二种**（段(7) 实施期发现，登记 Spec P.4）：`IkBackend`
//! 在 submit 期硬校验 `solver.info().joint_count == hardware.joints.len()`
//! （`ik.rs`）——6 维臂配置下任何 IK 提交都被 `FailedPrecondition` 拒，
//! AC-E18 的 **IK 分支**就测不到"终态后 reset 回 anchor"。RB5 已要求矩阵
//! **各分支独立 case 上下文**，故 IK 分支用 5 维 ctx，其余分支用 6 维。
//!
//! ## 清理
//!
//! `shutdown()` 逆序（gateway → s1 → s0）SIGTERM + grace；任一子进程非 0
//! 退出 → forensic dump + `Err`。`Drop` 由 `SubprocessHandle` 的
//! `kill_on_drop(true)` 兜底（test panic 路径）。

use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use thiserror::Error;
use tokio::sync::{Mutex as TokioMutex, MutexGuard};
use tonic::transport::Channel;

use robot_rpc::client::dual_arm_workflow::Side;

use crate::common::ready::{ReadyError, wait_ready};
use crate::common::subprocess::{
    MarkerError, ShutdownError, SubprocessHandle, build_gateway_command, build_s0_daemon_command,
    build_s1_daemon_command, locate_test_bin,
};

/// PD-E3 固定 hardware id。
pub const LEFT_HARDWARE_ID: &str = "left_arm";
pub const RIGHT_HARDWARE_ID: &str = "right_arm";

/// s0 就绪 marker：全部 worker + 两个 RR server 已 spawn（`s0_daemon.rs`
/// `set_enabled_rr_server started`），此后才有 state 发布可供 s1 锚定。
const S0_READY_MARKER: &str = "set_enabled_rr_server started";
const S0_MARKER_TIMEOUT: Duration = Duration::from_secs(8);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

/// 6 进程拓扑极重（2 × iceoryx2 service 组 + 2 个 loopback 端口），同一 test
/// binary 内**串行**跑（PD-E3）。
static GLOBAL_LOCK: TokioMutex<()> = TokioMutex::const_new(());
static CASE_SEQ: AtomicU32 = AtomicU32::new(0);

// ============================================================================
// 关节谱
// ============================================================================

/// 每臂关节向量形态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JointsSpec {
    /// D2(i) 6 维：5 臂关节 + gripper（AC-E15 夹爪同车用）。
    ArmWithGripper,
    /// 5 维，名字对齐 `XLeRobotArmIkSolver::info().joint_names`（IK 分支用）。
    IkSolver5,
}

impl JointsSpec {
    /// 关节名（`<side>` 前缀保持左右可区分，与权威 config 同构）。
    #[must_use]
    pub fn joint_names(self, side: Side) -> Vec<String> {
        let p = side.label();
        match self {
            Self::ArmWithGripper => vec![
                format!("{p}_arm_shoulder_pan"),
                format!("{p}_arm_shoulder_lift"),
                format!("{p}_arm_elbow_flex"),
                format!("{p}_arm_wrist_flex"),
                format!("{p}_arm_wrist_roll"),
                format!("{p}_arm_gripper"),
            ],
            // 名字必须 == solver info().joint_names（IkBackend 按 index 用，
            // 名字仅作 layout 派生；保持无前缀以贴合 solver 语义）。
            Self::IkSolver5 => vec![
                "shoulder_pan".into(),
                "shoulder_lift".into(),
                "elbow_flex".into(),
                "wrist_flex".into(),
                "wrist_roll".into(),
            ],
        }
    }

    /// 关节数。
    #[must_use]
    pub fn len(self) -> usize {
        match self {
            Self::ArmWithGripper => 6,
            Self::IkSolver5 => 5,
        }
    }

    /// gripper 在向量中的下标（仅 6 维形态有）。
    #[must_use]
    pub fn gripper_index(self) -> Option<usize> {
        match self {
            Self::ArmWithGripper => Some(5),
            Self::IkSolver5 => None,
        }
    }
}

/// bring-up 选项。
#[derive(Debug, Clone)]
pub struct DualCtxOptions {
    pub joints: JointsSpec,
    /// `[s1.profile] backends`。**默认含 `fault_inject` 且不含 `mock_dwell`**
    /// （R3-B1 冲突消除：两者 `(DwellMs, NO_PAYLOAD)` 分发键重合）。
    pub backends: Vec<&'static str>,
}

impl Default for DualCtxOptions {
    fn default() -> Self {
        Self {
            joints: JointsSpec::ArmWithGripper,
            backends: vec!["scripted", "hold_pose", "fault_inject"],
        }
    }
}

impl DualCtxOptions {
    /// IK 分支专用：5 维 solver 谱 + 启用 `ik`（不含 `fault_inject`）。
    #[must_use]
    pub fn ik_branch() -> Self {
        Self {
            joints: JointsSpec::IkSolver5,
            backends: vec!["scripted", "hold_pose", "ik"],
        }
    }

    /// 只跑 scripted / hold_pose（不需要 fault backend 的 case，可用未带
    /// `test-fault-backend` feature 的 s1 二进制）。
    #[must_use]
    pub fn scripted_only() -> Self {
        Self {
            joints: JointsSpec::ArmWithGripper,
            backends: vec!["scripted", "hold_pose"],
        }
    }
}

// ============================================================================
// 错误
// ============================================================================

#[derive(Debug, Error)]
pub enum DualBringUpError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("locate bin: {0}")]
    LocateBin(String),
    #[error("config: {0}")]
    Config(String),
    #[error("port allocation: {0}")]
    Port(String),
    #[error("{side} s0 marker: {source}")]
    S0Marker { side: Side, source: MarkerError },
    #[error("{side} ready: {source}")]
    Ready { side: Side, source: ReadyError },
    #[error("layout derive ({side}): {reason}")]
    Layout { side: Side, reason: String },
}

#[derive(Debug, Error)]
pub enum DualShutdownError {
    #[error("{side} {label} shutdown: {source}")]
    Shutdown {
        side: Side,
        label: &'static str,
        source: ShutdownError,
    },
    #[error("{side} {label} exit != 0: {status:?}")]
    ExitNonZero {
        side: Side,
        label: &'static str,
        status: std::process::ExitStatus,
    },
}

// ============================================================================
// ArmStack
// ============================================================================

/// 一条臂的三进程栈 + 元数据。
pub struct ArmStack {
    pub side: Side,
    pub robot_id: String,
    pub hardware_id: &'static str,
    pub joints: Vec<String>,
    /// config 派生的权威 layout（D1 链；与 s0 / s1 同值）。
    pub layout_id: u64,
    pub listen_addr: SocketAddr,
    pub config_path: PathBuf,
    pub latch_path: PathBuf,
    /// 已建立的 tonic Channel（caller 构造 `RobotControlClient`）。
    pub channel: Channel,

    s0: Option<SubprocessHandle>,
    s1: Option<SubprocessHandle>,
    gateway: Option<SubprocessHandle>,
}

impl ArmStack {
    /// s0 子进程 pid（wrong-layout / stale case 要对它 SIGSTOP）。
    #[must_use]
    pub fn s0_pid(&self) -> Option<u32> {
        self.s0.as_ref().and_then(SubprocessHandle::pid)
    }

    /// s0 state topic 路径（raw 注入器 / 观测用）。
    #[must_use]
    pub fn state_topic(&self) -> String {
        format!("/{}/s0/{}/state", self.robot_id, self.hardware_id)
    }

    /// s1 → s0 command topic 路径（R3-B4 raw 注入器目标）。
    #[must_use]
    pub fn command_topic(&self) -> String {
        format!("/{}/s1/{}/command", self.robot_id, self.hardware_id)
    }
}

// ============================================================================
// DualCtx
// ============================================================================

/// 双臂 6 进程上下文。**一个 test fn 一份**；`GLOBAL_LOCK` 保证串行。
pub struct DualCtx {
    pub left: ArmStack,
    pub right: ArmStack,
    pub forensic_dir: PathBuf,
    pub options: DualCtxOptions,
    _lock: MutexGuard<'static, ()>,
}

impl DualCtx {
    /// 默认选项（6 维臂 + scripted/hold_pose/fault_inject）。
    ///
    /// # Errors
    ///
    /// 见 [`DualBringUpError`]。
    pub async fn bring_up(test_name: &'static str) -> Result<Self, DualBringUpError> {
        Self::bring_up_with(test_name, DualCtxOptions::default()).await
    }

    /// 指定选项 bring-up。
    ///
    /// # Errors
    ///
    /// 见 [`DualBringUpError`]。
    pub async fn bring_up_with(
        test_name: &'static str,
        options: DualCtxOptions,
    ) -> Result<Self, DualBringUpError> {
        let lock = GLOBAL_LOCK.lock().await;
        let pid = std::process::id();
        let seq = CASE_SEQ.fetch_add(1, Ordering::Relaxed);
        let prefix = format!("dimos_e2e_{pid}_{seq}");
        let forensic_dir = PathBuf::from(format!("/tmp/dimos_e2e/{prefix}_{test_name}"));
        tokio::fs::create_dir_all(&forensic_dir).await?;

        // PD-E3：两端口均由 127.0.0.1:0 分配，**断言不同**。
        let left_addr = alloc_loopback_port()?;
        let right_addr = alloc_loopback_port()?;
        assert_ne!(
            left_addr, right_addr,
            "PD-E3: 双 gateway 必须占不同端口（left={left_addr}, right={right_addr}）"
        );

        let roles = write_roles_toml(&forensic_dir).await?;
        let left = bring_up_arm(
            Side::Left,
            &prefix,
            LEFT_HARDWARE_ID,
            left_addr,
            &forensic_dir,
            &roles,
            &options,
        )
        .await?;
        let right = bring_up_arm(
            Side::Right,
            &prefix,
            RIGHT_HARDWARE_ID,
            right_addr,
            &forensic_dir,
            &roles,
            &options,
        )
        .await?;

        Ok(Self {
            left,
            right,
            forensic_dir,
            options,
            _lock: lock,
        })
    }

    /// 按侧取臂栈。
    #[must_use]
    pub fn arm(&self, side: Side) -> &ArmStack {
        match side {
            Side::Left => &self.left,
            Side::Right => &self.right,
        }
    }

    /// 逆序停 6 进程（gateway → s1 → s0，左右各一组）；任一非 0 退出 → `Err`。
    ///
    /// # Errors
    ///
    /// 见 [`DualShutdownError`]。
    pub async fn shutdown(mut self) -> Result<(), DualShutdownError> {
        let mut first_err = None;
        for side in [Side::Left, Side::Right] {
            let arm = match side {
                Side::Left => &mut self.left,
                Side::Right => &mut self.right,
            };
            for (label, handle) in [
                ("gateway", arm.gateway.take()),
                ("s1_daemon", arm.s1.take()),
                ("s0_daemon", arm.s0.take()),
            ] {
                let Some(h) = handle else { continue };
                match h.shutdown(SHUTDOWN_GRACE).await {
                    Ok(status) if status.success() => {}
                    Ok(status) => {
                        first_err.get_or_insert(DualShutdownError::ExitNonZero {
                            side,
                            label,
                            status,
                        });
                    }
                    Err(source) => {
                        first_err.get_or_insert(DualShutdownError::Shutdown {
                            side,
                            label,
                            source,
                        });
                    }
                }
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

// ============================================================================
// 单臂 bring-up
// ============================================================================

async fn bring_up_arm(
    side: Side,
    prefix: &str,
    hardware_id: &'static str,
    listen_addr: SocketAddr,
    forensic_dir: &Path,
    roles_path: &Path,
    options: &DualCtxOptions,
) -> Result<ArmStack, DualBringUpError> {
    let robot_id = format!("{prefix}_{}", side.label());
    let joints = options.joints.joint_names(side);
    let config_path = forensic_dir.join(format!("{}_config.toml", side.label()));
    tokio::fs::write(
        &config_path,
        render_config(&robot_id, hardware_id, &joints, &options.backends),
    )
    .await?;

    // s0 strict fail-closed：latch primary + bak 都不存在会拒启动。
    let latch_path = forensic_dir.join(format!("{}_estop_latch", side.label()));
    tokio::fs::write(&latch_path, b"0\n").await?;
    tokio::fs::write(latch_path.with_extension("bak"), b"0\n").await?;

    let layout_id = robot_rpc::client::layout::load_hardware_layout(&config_path, hardware_id)
        .map_err(|e| DualBringUpError::Layout {
            side,
            reason: e.to_string(),
        })?
        .layout_id;

    let s0_bin = locate_test_bin("s0_daemon").map_err(DualBringUpError::LocateBin)?;
    let s1_bin = locate_test_bin("s1_daemon").map_err(DualBringUpError::LocateBin)?;
    let gw_bin = locate_test_bin("robot_rpc_gateway").map_err(DualBringUpError::LocateBin)?;

    // ---- s0（先起：s1 锚定要读它的 state）----
    let mut s0 = SubprocessHandle::spawn(
        "s0_daemon",
        build_s0_daemon_command(&s0_bin, &robot_id, hardware_id, &config_path, &latch_path),
    )?;
    s0.wait_for_marker(S0_READY_MARKER, S0_MARKER_TIMEOUT)
        .await
        .map_err(|source| DualBringUpError::S0Marker { side, source })?;

    // ---- s1 ----
    let mut s1 = SubprocessHandle::spawn(
        "s1_daemon",
        build_s1_daemon_command(&s1_bin, &robot_id, hardware_id, &config_path),
    )?;

    // ---- gateway（带 --config：state 池订阅 s0 state 供 GetState）----
    let mut gateway = SubprocessHandle::spawn(
        "gateway",
        build_gateway_command(
            &gw_bin,
            &robot_id,
            listen_addr,
            roles_path,
            Some(&config_path),
        ),
    )?;

    let channel = wait_ready(&mut s1, &mut gateway, listen_addr)
        .await
        .map_err(|source| DualBringUpError::Ready { side, source })?;

    Ok(ArmStack {
        side,
        robot_id,
        hardware_id,
        joints,
        layout_id,
        listen_addr,
        config_path,
        latch_path,
        channel,
        s0: Some(s0),
        s1: Some(s1),
        gateway: Some(gateway),
    })
}

// ============================================================================
// 资源 helper
// ============================================================================

fn alloc_loopback_port() -> Result<SocketAddr, DualBringUpError> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|e| DualBringUpError::Port(format!("bind 127.0.0.1:0: {e}")))?;
    let addr = listener
        .local_addr()
        .map_err(|e| DualBringUpError::Port(format!("local_addr: {e}")))?;
    drop(listener);
    Ok(addr)
}

/// per-case config：`[robot].id == --robot-id`（段(2) 身份锁）+ mock_hard_rt
/// + profile。
fn render_config(
    robot_id: &str,
    hardware_id: &str,
    joints: &[String],
    backends: &[&'static str],
) -> String {
    let joint_list = joints
        .iter()
        .map(|j| format!("\"{j}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let motor_ids = (1..=joints.len())
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let backend_list = backends
        .iter()
        .map(|b| format!("\"{b}\""))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "# v0.5 候选 E 段(7) dimos_workflow_e2e 自动生成（per-case，勿手改）。\n\
         [robot]\n\
         id             = \"{robot_id}\"\n\
         config_version = \"0.5.0\"\n\
         \n\
         [[hardware]]\n\
         id          = \"{hardware_id}\"\n\
         adapter     = \"mock_hard_rt\"\n\
         joints      = [{joint_list}]\n\
         motor_ids   = [{motor_ids}]\n\
         auto_enable = true\n\
         \n\
         [s1.profile]\n\
         backends     = [{backend_list}]\n\
         command_mode = \"joint_position\"\n"
    )
}

/// 复制仓库 `configs/rpc_roles.toml` 到 forensic dir（gateway `--rpc-roles-toml`）。
async fn write_roles_toml(dir: &Path) -> Result<PathBuf, DualBringUpError> {
    let out = dir.join("rpc_roles.toml");
    let src = repo_root().join("configs/rpc_roles.toml");
    let content = tokio::fs::read(&src)
        .await
        .map_err(|e| DualBringUpError::Config(format!("read roles toml {}: {e}", src.display())))?;
    tokio::fs::write(&out, content).await?;
    Ok(out)
}

/// workspace 根（`CARGO_MANIFEST_DIR` = crates/robot-rpc → 上两级）。
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}
