//! `DualCtx`：**2×(s0 + s1 + gateway) = 6 子进程**双臂 e2e harness
//! （v0.5 候选 E 段(7)，Spec PD-E3 + D4 + D9 "三真同堂首例"）。
//!
//! ## 跟既有 `CaseCtx`（B4，`task_rpc_e2e.rs`）的差别
//!
//! | 项 | `CaseCtx` | `DualCtx` |
//! |---|---|---|
//! | 拓扑 | FakeState + s1 + gateway（3 子进程/线程） | **真 s0** + s1 + gateway **×2 臂** |
//! | state 源 | `FakeStatePublisher`（测试线程） | **真 robot-s0**（mock_hard_rt adapter） |
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
//! 3. spawn **robot-s0** → 等 marker `set_enabled_rr_server started`
//!    （s0 全部线程 + 两个 RR server 就绪；早于它 spawn s1 会让 s1 锚定
//!    读不到 state → exit 4）；
//! 4. spawn **robot-s1**（启动锚定 + 段(5) state cache 订阅 s0 state）；
//! 5. spawn **gateway**（带 `--config`，state 池订阅 s0 state 供 `GetState`）；
//! 6. `wait_ready` 三段式（s1 marker → gateway marker → TCP → RPC probe）。
//! 7. 独立等待 gateway 的首个合法 `GetState`：任务 RPC 已通不代表 state
//!    订阅池已收到首帧；只在初始化阶段重试明确的无首帧响应。
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
//! **IK 分支的宣称边界（round 1 issue 5，务必照此读结果）**：
//!
//! 1. `robot-s1` 目前**固定**构造 `XLeRobotArmIkSolver::new(Side::Right)`
//!    （`robot-s1.rs`，无左右开关）——因此 IK 分支**只在右臂执行**，左臂
//!    即便配 5 维也是拿右臂运动学在算；
//! 2. 该分支**只验证 D3 的 reset 规则**（IK 终态后 Idle hold target 回启动
//!    anchor），**不**代表权威 6 维配置支持 IK，**不**证明左臂 IK 正确，
//!    **不**是 IK 数值精度的验收（那属 v0.4 候选 2 的 unit / smoke 面）。
//!
//! ## 清理
//!
//! `shutdown()` 逆序（gateway → s1 → s0）SIGTERM + grace；**每个子进程的
//! stdout/stderr 都 dump 到 `forensic_dir/<side>_<label>.log`**（`shutdown`
//! 后写，含停机期输出），任一子进程非 0 退出 → `Err`。`Drop` 由
//! `SubprocessHandle` 的 `kill_on_drop(true)` 兜底（test panic 路径）。

use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use thiserror::Error;
use tokio::sync::{Mutex as TokioMutex, MutexGuard};
use tonic::transport::Channel;

use robot_core::time::{DurationNs, RobotClock, SystemClock};
use robot_rpc::client::dual_arm_workflow::{PairConfig, Side};
use robot_rpc::client::layout::{HardwareLayout, validate_state_layout};
use robot_rpc::proto::robot_control_client::RobotControlClient;
use robot_rpc::proto::{GetStateRequest, RobotState};

use crate::common::ready::{ReadyError, wait_ready};
use crate::common::subprocess::{
    MarkerError, ShutdownError, SubprocessHandle, build_gateway_command, build_s0_daemon_command,
    build_s1_daemon_command, locate_test_bin,
};

/// PD-E3 固定 hardware id。
pub const LEFT_HARDWARE_ID: &str = "left_arm";
pub const RIGHT_HARDWARE_ID: &str = "right_arm";

/// s0 就绪 marker：全部 worker + 两个 RR server 已 spawn（`robot-s0.rs`
/// `set_enabled_rr_server started`），此后才有 state 发布可供 s1 锚定。
const S0_READY_MARKER: &str = "set_enabled_rr_server started";
const S0_MARKER_TIMEOUT: Duration = Duration::from_secs(8);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);
/// 与 `ready` 的单阶段 RPC 就绪预算一致；不计入轨迹执行或终态预算。
const FIRST_STATE_TIMEOUT: DurationNs = DurationNs::from_secs(5);

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

/// `fault_inject` 与 `mock_dwell` 分发键完全重合（`DwellMs` +
/// `NO_PAYLOAD_SCHEMA_ID`），共存时 dispatch 只看 enabled 列表**首个命中**
/// ——failure 分支会静默失效。启动前拦掉（round 1 issue 4）。
const FAULT_BACKEND: &str = "fault_inject";
const CONFLICTING_BACKEND: &str = "mock_dwell";

impl DualCtxOptions {
    /// profile 合法性（bring-up 前调）。
    ///
    /// # Errors
    ///
    /// `fault_inject` 与 `mock_dwell` 同时出现 → `Err`（R3-B1 冲突消除）。
    pub fn validate(&self) -> Result<(), String> {
        let has_fault = self.backends.contains(&FAULT_BACKEND);
        let has_mock = self.backends.contains(&CONFLICTING_BACKEND);
        if has_fault && has_mock {
            return Err(format!(
                "[s1.profile] backends 同时含 {FAULT_BACKEND:?} 与 {CONFLICTING_BACKEND:?}：\
                 两者 (DwellMs, NO_PAYLOAD_SCHEMA_ID) 分发键重合，dispatch 取首个命中，\
                 failure 分支会静默失效（Spec R3-B1）。请移除 {CONFLICTING_BACKEND:?}。\
                 实得 {:?}",
                self.backends
            ));
        }
        if self.backends.is_empty() {
            return Err("[s1.profile] backends 不能为空".to_string());
        }
        Ok(())
    }

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
    #[error("{side} first state: {reason}")]
    FirstState { side: Side, reason: String },
    #[error("layout derive ({side}): {reason}")]
    Layout { side: Side, reason: String },
    /// `DualCtxOptions::validate` 失败（如 fault_inject / mock_dwell 共存）。
    #[error("invalid options: {0}")]
    InvalidOptions(String),
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

/// 取全局串行锁（6 进程拓扑极重）。`DualCtx` 内部已持有；**不经 `DualCtx`
/// 的重量级场景**（如 AC-E2 的 s0-only mini 拓扑）也应显式取，避免与双臂
/// case 抢 CPU / iceoryx2 资源。
pub async fn global_lock() -> MutexGuard<'static, ()> {
    GLOBAL_LOCK.lock().await
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
        options
            .validate()
            .map_err(DualBringUpError::InvalidOptions)?;
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
                ("robot-s1", arm.s1.take()),
                ("robot-s0", arm.s0.take()),
            ] {
                let Some(h) = handle else { continue };
                // forensic：dump 到 <forensic_dir>/<side>_<label>.log（含停机
                // 期输出——`shutdown_with_dump` 在 wait 之后才写）。
                let dump_path = self
                    .forensic_dir
                    .join(format!("{}_{label}.log", side.label()));
                match h.shutdown_with_dump(SHUTDOWN_GRACE, dump_path).await {
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

    let layout = robot_rpc::client::layout::load_hardware_layout(&config_path, hardware_id)
        .map_err(|e| DualBringUpError::Layout {
            side,
            reason: e.to_string(),
        })?;

    let s0_bin = locate_test_bin("robot-s0").map_err(DualBringUpError::LocateBin)?;
    let s1_bin = locate_test_bin("robot-s1").map_err(DualBringUpError::LocateBin)?;
    let gw_bin = locate_test_bin("robot-gateway").map_err(DualBringUpError::LocateBin)?;

    // ---- s0（先起：s1 锚定要读它的 state）----
    let mut s0 = SubprocessHandle::spawn(
        "robot-s0",
        build_s0_daemon_command(&s0_bin, &robot_id, hardware_id, &config_path, &latch_path),
    )?;
    s0.wait_for_marker(S0_READY_MARKER, S0_MARKER_TIMEOUT)
        .await
        .map_err(|source| DualBringUpError::S0Marker { side, source })?;

    // ---- s1 ----
    let mut s1 = SubprocessHandle::spawn(
        "robot-s1",
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
    let expected = FirstStateExpected {
        robot_id: &robot_id,
        hardware_id,
        layout: &layout,
    };
    wait_first_state_rpc(&channel, &expected)
        .await
        .map_err(|reason| DualBringUpError::FirstState { side, reason })?;

    Ok(ArmStack {
        side,
        robot_id,
        hardware_id,
        joints,
        layout_id: layout.layout_id,
        listen_addr,
        config_path,
        latch_path,
        channel,
        s0: Some(s0),
        s1: Some(s1),
        gateway: Some(gateway),
    })
}

/// `RobotState` 不带关节名；配置派生 layout 同时锁定关节顺序和 motor id。
struct FirstStateExpected<'a> {
    robot_id: &'a str,
    hardware_id: &'a str,
    layout: &'a HardwareLayout,
}

impl FirstStateExpected<'_> {
    fn validate(&self, state: &RobotState) -> Result<(), String> {
        if state.robot_id != self.robot_id || state.hardware_id != self.hardware_id {
            return Err(format!(
                "首帧身份不匹配：期望 {}/{}, 实得 {}/{}",
                self.robot_id, self.hardware_id, state.robot_id, state.hardware_id,
            ));
        }
        validate_state_layout(self.layout, state).map_err(|error| error.to_string())?;
        if state.position_rad.len() != self.layout.joints.len() {
            return Err(format!(
                "首帧关节长度不匹配：期望 {}, 实得 {}",
                self.layout.joints.len(),
                state.position_rad.len(),
            ));
        }
        if !state.position_rad.iter().all(|value| value.is_finite()) {
            return Err("首帧位置含非有限值".to_string());
        }
        Ok(())
    }
}

async fn wait_first_state_rpc(
    channel: &Channel,
    expected: &FirstStateExpected<'_>,
) -> Result<(), String> {
    let mut client = RobotControlClient::new(channel.clone());
    wait_first_state(
        &SystemClock::new(),
        expected,
        FIRST_STATE_TIMEOUT,
        async |remaining| {
            let mut request = tonic::Request::new(GetStateRequest {
                hardware_id: expected.hardware_id.to_string(),
            });
            request.set_timeout(remaining);
            client
                .get_state(request)
                .await
                .map(tonic::Response::into_inner)
        },
        async |duration| tokio::time::sleep(duration).await,
    )
    .await
}

/// 真实 RPC 与回归夹具共用同一轮询逻辑；只把已知 hardware 的无首帧视为可重试。
async fn wait_first_state(
    clock: &impl RobotClock,
    expected: &FirstStateExpected<'_>,
    total: DurationNs,
    mut read: impl AsyncFnMut(Duration) -> Result<RobotState, tonic::Status>,
    mut wait: impl AsyncFnMut(Duration),
) -> Result<(), String> {
    let deadline = clock.now() + total;
    let no_frame_message = format!(
        "hardware_id '{}' has no S0 state sample yet",
        expected.hardware_id,
    );
    // 沿用双臂工作流已有的 RPC 轮询频率；合法首帧直接返回，无固定 settle 等待。
    let retry_interval = PairConfig::default().poll_interval;
    let timeout_reason = || {
        format!(
            "等待 {}/{} 首帧超过 {} ms（RPC 与轮询共用总截止）",
            expected.robot_id,
            expected.hardware_id,
            total.as_nanos() / 1_000_000,
        )
    };
    loop {
        let remaining = deadline.0.saturating_sub(clock.now().0);
        if remaining == 0 {
            return Err(timeout_reason());
        }
        let result = tokio::time::timeout(
            Duration::from_nanos(remaining),
            read(Duration::from_nanos(remaining)),
        )
        .await;
        // RPC 恰在截止处完成也不能延长初始化总预算。
        let remaining = deadline.0.saturating_sub(clock.now().0);
        if remaining == 0 {
            return Err(timeout_reason());
        }
        match result {
            Ok(Ok(state)) => return expected.validate(&state),
            Ok(Err(status))
                if status.code() == tonic::Code::Unavailable
                    && status.message() == no_frame_message => {}
            Ok(Err(status)) => return Err(format!("首帧 RPC 失败：{status}")),
            Err(_) => return Err(timeout_reason()),
        }
        wait(retry_interval.min(Duration::from_nanos(remaining))).await;
    }
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

/// 复制仓库 `configs/rpc-roles.toml` 到 forensic dir（gateway `--rpc-roles-toml`）。
async fn write_roles_toml(dir: &Path) -> Result<PathBuf, DualBringUpError> {
    let out = dir.join("rpc-roles.toml");
    let src = repo_root().join("configs/rpc-roles.toml");
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

#[cfg(test)]
mod first_state_tests {
    use super::*;
    use robot_core::time::MockClock;
    use std::cell::Cell;

    fn layout() -> HardwareLayout {
        HardwareLayout {
            joints: vec!["joint_0".to_string(), "joint_1".to_string()],
            motor_ids: vec![1, 2],
            layout_id: 7,
        }
    }

    fn expected(layout: &HardwareLayout) -> FirstStateExpected<'_> {
        FirstStateExpected {
            robot_id: "test_robot",
            hardware_id: "right_arm",
            layout,
        }
    }

    fn state() -> RobotState {
        RobotState {
            robot_id: "test_robot".to_string(),
            hardware_id: "right_arm".to_string(),
            layout_id: 7,
            position_rad: vec![0.0, 0.0],
            ..RobotState::default()
        }
    }

    fn no_frame() -> tonic::Status {
        tonic::Status::unavailable("hardware_id 'right_arm' has no S0 state sample yet")
    }

    #[tokio::test]
    async fn first_state_waits_for_a_valid_sample_after_no_frame() {
        let clock = MockClock::default();
        let layout = layout();
        let reads = Cell::new(0);
        let waits = Cell::new(0);
        wait_first_state(
            &clock,
            &expected(&layout),
            FIRST_STATE_TIMEOUT,
            async |_| {
                reads.set(reads.get() + 1);
                match reads.get() {
                    1 => Err(no_frame()),
                    2 => Ok(state()),
                    _ => panic!("合法首帧后不应继续查询"),
                }
            },
            async |duration| {
                waits.set(waits.get() + 1);
                clock.advance(DurationNs::from_nanos(
                    u64::try_from(duration.as_nanos()).unwrap(),
                ));
            },
        )
        .await
        .unwrap();
        assert_eq!(reads.get(), 2);
        assert_eq!(waits.get(), 1, "合法首帧后没有额外 settle");
    }

    #[tokio::test]
    async fn first_state_rejects_invalid_payload_without_retry() {
        let mut invalid = vec![state(); 5];
        invalid[0].robot_id = "other_robot".to_string();
        invalid[1].hardware_id = "left_arm".to_string();
        invalid[2].layout_id = 8;
        let _ = invalid[3].position_rad.pop();
        invalid[4].position_rad[0] = f64::NAN;
        let layout = layout();
        for sample in invalid {
            let reads = Cell::new(0);
            let result = wait_first_state(
                &MockClock::default(),
                &expected(&layout),
                FIRST_STATE_TIMEOUT,
                async |_| {
                    reads.set(reads.get() + 1);
                    Ok(sample.clone())
                },
                async |_| panic!("非法首帧不可当作尚未就绪重试"),
            )
            .await;
            assert!(result.is_err(), "非法首帧被接受：{sample:?}");
            assert_eq!(reads.get(), 1);
        }
    }

    #[tokio::test]
    async fn first_state_retries_only_the_expected_no_frame_status() {
        let layout = layout();
        for status in [
            tonic::Status::unavailable("transport connection closed"),
            tonic::Status::unavailable("hardware_id 'left_arm' has no S0 state sample yet"),
            tonic::Status::not_found("unknown hardware"),
        ] {
            let result = wait_first_state(
                &MockClock::default(),
                &expected(&layout),
                FIRST_STATE_TIMEOUT,
                async |_| Err(status.clone()),
                async |_| panic!("其他 RPC 故障必须立即失败"),
            )
            .await;
            assert!(result.unwrap_err().contains("首帧 RPC 失败"));
        }
    }

    #[tokio::test]
    async fn first_state_no_frame_uses_one_deadline_for_read_and_wait() {
        let clock = MockClock::default();
        let layout = layout();
        let reads = Cell::new(0);
        let result = wait_first_state(
            &clock,
            &expected(&layout),
            DurationNs::from_millis(5),
            async |remaining| {
                reads.set(reads.get() + 1);
                assert_eq!(remaining, Duration::from_millis(5));
                clock.advance(DurationNs::from_millis(4));
                Err(no_frame())
            },
            async |duration| {
                assert_eq!(duration, Duration::from_millis(1));
                clock.advance(DurationNs::from_millis(1));
            },
        )
        .await;
        assert!(result.unwrap_err().contains("超过 5 ms"));
        assert_eq!(reads.get(), 1, "到总截止后禁止开始第二次 RPC");
    }

    #[tokio::test]
    async fn first_state_pending_rpc_is_bounded_by_the_total_deadline() {
        let layout = layout();
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            wait_first_state(
                &MockClock::default(),
                &expected(&layout),
                DurationNs::from_millis(1),
                async |remaining| {
                    assert_eq!(remaining, Duration::from_millis(1));
                    std::future::pending().await
                },
                async |_| panic!("悬挂 RPC 超时后不能再等待或重试"),
            ),
        )
        .await
        .expect("初始化总预算必须截断永不返回的 RPC");
        assert!(result.unwrap_err().contains("超过 1 ms"));
    }
}
