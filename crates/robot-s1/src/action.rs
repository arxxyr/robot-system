//! v0.3 Phase B B2 action interface 类型骨架 + Coordinator API 表层类型。
//!
//! **变更历史**：
//!
//! - **B2 (1/3)** (`4b9d4de` + reviewer fix `54521ed` / `8508c3d`)：lib 纯
//!   类型 + trait 定义初版（[`ActionId`] / [`ActionKind`] /
//!   [`ActionRequest`] / [`ActionBackend`] / [`ActionInstance`] /
//!   [`JointCommandBody`] / 错误枚举）。
//! - **B2 (2/3)**：[`SubmitError::NotReady`]（Booting 拒 submit）+
//!   [`ActionTicket`] / [`ActionEvent`] / [`ActionStatus`] coordinator API
//!   表层（详
//!   [`docs/handoffs/v0.3_b2_outline.md`](../../../docs/handoffs/v0.3_b2_outline.md)
//!   §7 + reviewer 拍板补充）。
//!
//! 本模块仍**只**含 lib 类型 + trait 定义；backend 实现 /
//! `BackendRegistry` / `Coordinator` wiring 在
//! [`crate::action_backend`] / [`crate::coordinator`] 落地，**不**进本文件
//! （B2 (2/3) reviewer 文件边界锁定）。
//!
//! ## 模块入口
//!
//! - [`ActionId`]：UUIDv7 newtype（§10 决策 #2）。
//! - [`ActionKind`]：B2 enum 仅 [`ActionKind::DwellMs`] +
//!   [`ActionKind::HoldPose`] 两个 variant（§10 决策 #1 + 阻断 3）；
//!   `Stop` / `MoveToPose` / `FollowTrajectory` / `PolicyAction`
//!   **不**进 B2 enum。
//! - [`ActionRequest`] + [`ActionRequest::validate`]：纯字段校验
//!   （deadline / idempotency_key / payload 长度上限 + 哨兵 schema_id
//!   守门）。
//! - [`NO_PAYLOAD_SCHEMA_ID`]：`payload_schema_id` 哨兵值（reviewer 阻
//!   断 1 锁定，去掉 [`ActionBackend::supported_schemas`] 空切片魔法语
//!   义）。
//! - [`JointCommandBody`]：`Vec<u8>` newtype，让"这是 JointCommand body
//!   字节流"在类型上一目了然（§10 决策 #8）。
//! - [`ActionBackend`] / [`ActionInstance`] trait：B2 (2/3) 起按
//!   `inventory::submit!` 静态注册到
//!   [`crate::action_backend::BackendRegistry`]。
//! - [`ActionContext`]：tick 时由 coordinator 借给 backend 的上下文
//!   （`RobotClock` / `HardwareSection` / `CoordinatorConfig` /
//!   `anchor_bias` / v0.5 候选 E 段(5) 起的 `scripted_start_pose`）；B2
//!   **不**暴露 publisher trait object（§10 决策 #8 + outline §2.5 注释）。
//! - [`ActionInstance::successful_hold_target`]（v0.5 候选 E 段(5)，Spec
//!   D3 第 3 条 + QA2）：终态数据通路——**仅** Scripted 在末 waypoint
//!   target 已发布后返 `Some`；默认 `None` 让其余 backend 天然走
//!   `idle_hold_target = anchor_bias` reset（穷尽式单一规则由类型承担）。
//! - 错误枚举：[`SubmitError`] / [`ActionStartError`] / [`ActionCancelError`]
//!   / [`ClearError`] / [`ActionFailure`]。`SubmitError::NotReady` =
//!   B2 (2/3) 加，对应 `Booting` 状态拒 submit（reviewer 拍板补充：
//!   不复用 `ShuttingDown`，因两阶段生命周期相反）。
//! - [`ActionTicket`] / [`ActionEvent`] / [`ActionStatus`]：coordinator
//!   `submit()` 表层类型；事件 channel 用 `std::sync::mpsc::Receiver`
//!   （reviewer 阻断 1 锁定，**不**用 tokio；B4 时 robot-rpc-gateway 起
//!   adapter thread 桥到 tokio）。
//!
//! ## 不做（B2 边界）
//!
//! - **不**实现 backend / [`crate::action_backend::BackendRegistry`] /
//!   `parse_profile_section_from_toml` 自身：这些落
//!   [`crate::coordinator`] + [`crate::action_backend`] 模块（B2 (2/3)
//!   同 commit chain，**不**进本文件）。
//! - **不**改 `s1_daemon` binary（→ B2 (3/3)）。
//! - **不**引入跨进程 fbs schema：`payload: Vec<u8>` 字段先占位，B4
//!   真接 RPC `InvokeTask` 时再引入 `action_request.fbs` /
//!   `action_status.fbs`（§10 决策 #9）。

// 中文 doc 块大量 identifier-like 字串（mock_s1 / s1_daemon /
// coordinator / RobotClock 等），跟 lib 同款宽松 doc-markdown。
#![allow(clippy::doc_markdown)]

use robot_core::time::RobotClock;
use robot_kinematics::ArmIkSolver;
use thiserror::Error;
use uuid::Uuid;

use crate::coordinator::{CoordinatorConfig, HardwareSection};

// ============================================================================
// 字段上限常量
// ============================================================================

/// `ActionRequest.payload` 字节数上限（B2 mock backend 用空，B3 起
/// trajectory / policy chunk 等 backend 才会用；超此值 [`SubmitError::InvalidRequest`]
/// 拒绝）。1 MiB 是宽松上限，远超任何合理 in-process payload。
pub const MAX_PAYLOAD_LEN: usize = 1024 * 1024;

/// `ActionRequest.idempotency_key` 字节数上限。RPC `CommandMeta.idempotency_key`
/// 通常是 UUID 或短哈希；256 字节足够覆盖，超此值视为客户端 bug 拒绝。
pub const MAX_IDEMPOTENCY_KEY_LEN: usize = 256;

/// "无 payload" 的 schema_id 哨兵值（reviewer 阻断 1 锁定）。
///
/// 约定：
///
/// - 空 payload backend（如 `MockDwell`）**必须**在
///   [`ActionBackend::supported_schemas`] 显式返回 `&[NO_PAYLOAD_SCHEMA_ID]`，
///   不能返回空切片做隐式 "any payload" 处理。
/// - [`ActionBackend::supported_schemas`] 返回空切片 = 该 backend 不接
///   受任何 schema（理论上 dispatcher 永远找不到匹配，等价于没注册；
///   仅作为退化态保留，不应在生产 backend 出现）。
/// - [`ActionRequest::validate`] 强制 `payload_schema_id == NO_PAYLOAD_SCHEMA_ID`
///   时 `payload` 必须为空 —— 防止客户端用 schema_id 0 把任意 bytes 偷
///   渡进来。
///
/// 数值固定为 0：跟 RPC `CommandMeta.payload_schema_id` 默认值（未设置
/// 字段 = 0）天然对齐，零 payload 调用方不需要显式填值。
pub const NO_PAYLOAD_SCHEMA_ID: u32 = 0;

// ============================================================================
// ActionId
// ============================================================================

/// UUIDv7 newtype（§10 决策 #2 拍板：跟 RPC `CommandMeta.request_id`
/// 同款时间排序，B4 时 robot-rpc-gateway 把 `request_id` 直接转成
/// `ActionId`，无需翻译）。
///
/// `pub` 内部 [`Uuid`] 让单测 / B4 gateway 端可以从已存在的 UUIDv7 直接
/// 构造 `ActionId(uuid)`，不强制走 `new_v7()` 路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ActionId(pub Uuid);

impl ActionId {
    /// 生成一个新的 UUIDv7 `ActionId`（time-ordered；前 48 bit 是 ms
    /// since UNIX epoch）。
    ///
    /// **不**在 RT 线程调用（`uuid::Uuid::now_v7` 走系统时钟 + RNG，
    /// allocator 在 v7 编码路径里间接被触发）。B2 (1/3) action 路径全部
    /// 在 coordinator IPC 线程，非 RT。
    #[must_use]
    pub fn new_v7() -> Self {
        Self(Uuid::now_v7())
    }
}

impl core::fmt::Display for ActionId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ============================================================================
// ActionKind
// ============================================================================

/// B2 action 类型枚举。
///
/// **reviewer §10 决策 #1 + 阻断 3 锁定**：B2 仅 2 个 variant；
/// **不**放 `Stop`（混淆 task cancel / S1 degraded / S0 control-lost
/// stop / EStop 4 个语义层）；**不**放 B3 占位 variant
/// （`MoveToPose` / `FollowTrajectory` / `PolicyAction`）—— B3 落地对
/// 应 backend 时再扩 enum，向 `docs/00_design.md` §6.6.4 errata 边界
/// 对齐。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActionKind {
    /// Dwell N ms 后完成；不输出新 `JointCommand`，coordinator 沿用
    /// hold bias。B2 mock backend 唯一支持值之一。
    DwellMs,
    /// 保持当前 anchor，等价 IDLE 但走 EXECUTING 状态路径（用于测试
    /// state machine 转移而不动 wire 输出）。B2 mock backend 唯一支持
    /// 值之一。
    HoldPose,
    /// joint-space 线性插值 + waypoint dwell 序列（v0.4 候选 1 land）。
    /// 详 `docs/handoffs/v0.4_entry.md` §3.2 +
    /// `team-swe/specs/v0_4_scripted_backend.md`。Payload 走
    /// `SCHEMA_SCRIPTED_PAYLOAD` (= `0x0000_040B`)；backend impl 落
    /// `crates/robot-s1/src/action_backend/scripted.rs` (2/3 land)。
    Scripted,
    /// 笛卡尔 pose IK 解算 + 关节位置输出（v0.4 候选 2 round 2 land）。
    /// 详 `team-swe/specs/v0_4_robot_kinematics_round2.md` Q-IK1..Q-IK17。
    /// Payload 走 `SCHEMA_IK_PAYLOAD` (= `0x0000_040C`)；backend impl
    /// 落 `crates/robot-s1/src/action_backend/ik.rs`（round 2 (3/4)
    /// 待 land）；solver impl 走 `crates/robot-kinematics-xlerobot`
    /// (round 2 (1/4) land)。
    Ik,
}

// ============================================================================
// JointCommandBody
// ============================================================================

/// `JointCommand` body 字节流的 newtype 包装（§10 决策 #8 拍板 —— 不
/// 裸 `Vec<u8>`）。
///
/// 让 `ActionTickOutput::Continue::command` 在类型上明确是 "JointCommand
/// body"，防 backend 误塞任意 bytes；coordinator 拿到后直接调
/// `SampleWriter::set_body_bytes(body.as_bytes())`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JointCommandBody(Vec<u8>);

impl JointCommandBody {
    /// 用现成的 `JointCommand` body bytes 构造（典型来自
    /// [`crate::coordinator::build_hold_command`] 或 backend 内部
    /// `build_joint_command()`）。
    #[must_use]
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    /// 借用底层 bytes（`coordinator` publish 路径 zero-copy 调用）。
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// 按值消费返底层 `Vec<u8>`。
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    /// body 字节长度。
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// body 字节是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

// ============================================================================
// ActionRequest
// ============================================================================

/// 一次 action 提交请求。
///
/// 字段语义：
///
/// - `action_id`：UUIDv7 全局 ID。B4 时直接复用 RPC `CommandMeta.request_id`。
/// - `kind`：路由用；coordinator 按 `kind` 找 backend
///   （[`ActionBackend::supported_kinds`]）。
/// - `deadline_ns`：monotonic deadline（`CLOCK_MONOTONIC_RAW`）；调用方
///   （B4 时是 gateway）转 RPC `timeout_ms` 为 `now_ns + timeout_ms * 1e6`。
/// - `priority`：同时多 action 时大者优先；B2 单 action 序列化（§10
///   决策 #3）所以本字段保留**不**参与决策，B3 scheduler 用。
/// - `payload_schema_id`：路由二级 key；backend 注册时声明
///   [`ActionBackend::supported_schemas`]，dispatcher 按
///   `(kind, schema_id)` 路由。**[`NO_PAYLOAD_SCHEMA_ID`] 表示"本请求
///   无 payload"**，[`ActionRequest::validate`] 强制此时 `payload` 为
///   空（B2 mock backend / DwellMs / HoldPose 走这条路径）。
/// - `payload`：`FlatBuffers` body bytes（按 `payload_schema_id`）。
///   **B2 mock backend 用空**（配 [`NO_PAYLOAD_SCHEMA_ID`]）；B4 才引
///   入跨进程 fbs schema（§10 决策 #9）。
/// - `idempotency_key`：跨 retry 一致 fingerprint，与 RPC
///   `CommandMeta.idempotency_key` 对齐。**B2 仅**声明字段，**不**做
///   dedup（B4 gateway 自己有 idempotency cache，详 A1 ClearEStop 边界）。
/// - `source`：给 audit / recorder。
#[derive(Debug, Clone)]
pub struct ActionRequest {
    pub action_id: ActionId,
    pub kind: ActionKind,
    pub deadline_ns: u64,
    pub priority: u8,
    pub payload_schema_id: u32,
    pub payload: Vec<u8>,
    pub idempotency_key: String,
    pub source: ActionSource,
}

impl ActionRequest {
    /// 纯字段校验（不依赖 coordinator state / backend registry）。
    ///
    /// **检查项**：
    /// - `deadline_ns > now_ns`（提交时未过期）。
    /// - `idempotency_key` 非空且 ≤ [`MAX_IDEMPOTENCY_KEY_LEN`]。
    /// - `payload.len()` ≤ [`MAX_PAYLOAD_LEN`]。
    /// - `payload_schema_id == ` [`NO_PAYLOAD_SCHEMA_ID`] 时 `payload`
    ///   必须为空（reviewer 阻断 1 锁定 —— 防止客户端用哨兵 schema_id
    ///   偷渡任意 bytes）。
    ///
    /// **不检查**：
    /// - `kind` 是否有匹配 backend（路由查表是 coordinator 的事，留 B2 (2/3)）。
    /// - 非哨兵 `payload_schema_id` 是否在 backend `supported_schemas()`
    ///   集合（同上）。
    /// - `action_id` 唯一性（B2 单 action 序列化，state 守门即可）。
    ///
    /// 返 [`SubmitError::InvalidRequest`]，`String` 是 human-readable
    /// 字段名 + 原因，便于 gateway 透传给客户端 audit。
    pub fn validate(&self, now_ns: u64) -> Result<(), SubmitError> {
        if self.deadline_ns <= now_ns {
            return Err(SubmitError::InvalidRequest(format!(
                "deadline_ns ({}) must be > now_ns ({})",
                self.deadline_ns, now_ns
            )));
        }
        if self.idempotency_key.is_empty() {
            return Err(SubmitError::InvalidRequest(
                "idempotency_key must not be empty".into(),
            ));
        }
        if self.idempotency_key.len() > MAX_IDEMPOTENCY_KEY_LEN {
            return Err(SubmitError::InvalidRequest(format!(
                "idempotency_key length {} exceeds MAX_IDEMPOTENCY_KEY_LEN ({})",
                self.idempotency_key.len(),
                MAX_IDEMPOTENCY_KEY_LEN
            )));
        }
        if self.payload.len() > MAX_PAYLOAD_LEN {
            return Err(SubmitError::InvalidRequest(format!(
                "payload length {} exceeds MAX_PAYLOAD_LEN ({})",
                self.payload.len(),
                MAX_PAYLOAD_LEN
            )));
        }
        if self.payload_schema_id == NO_PAYLOAD_SCHEMA_ID && !self.payload.is_empty() {
            return Err(SubmitError::InvalidRequest(format!(
                "payload_schema_id == NO_PAYLOAD_SCHEMA_ID ({}) requires empty payload, got {} bytes",
                NO_PAYLOAD_SCHEMA_ID,
                self.payload.len()
            )));
        }
        Ok(())
    }
}

// ============================================================================
// ActionSource
// ============================================================================

/// `ActionRequest.source` 来源标识（给 audit / recorder）。
#[derive(Debug, Clone)]
pub enum ActionSource {
    /// 来自 `robot-rpc` gateway（B4 起接 tonic `InvokeTask`）。
    /// `actor` 是 RPC `CommandMeta.source`（如 `"cli.loosqk"`）。
    RpcGateway { actor: String },
    /// coordinator 自己排程（B2 不构造，B3+ scheduler 用）。
    Internal,
}

// ============================================================================
// ActionContext
// ============================================================================

/// Backend `tick()` 时由 coordinator 借给 backend 的上下文。
///
/// **B2 (1/3) 字段集**（**不**暴露 publisher / 最新 S0 state；§10 决策
/// #8 锁定 —— backend 经 `ActionTickOutput::Continue::command` 返
/// [`JointCommandBody`] 让 coordinator publish，最干净）：
///
/// - `clock`：trait object，测试用 `MockClock` 推时间，binary 用
///   `SystemClock`。
/// - `layout_id`：joint layout fingerprint，backend 写
///   `JointCommand.layout_id` 时用。
/// - `hardware`：B1 (1/3) [`HardwareSection`]（`id` + `joints`）。
/// - `config`：B1 (1/3) [`CoordinatorConfig`]（`heartbeat_hz` /
///   `command_hz` / `deadline_ms` / 等）。
/// - `anchor_bias`：启动锚定的 bias（构造期固化，**不再变**），供
///   `HoldPose` 每 tick 读作真实输出目标（v0.5 候选 E Spec D3 第 5 条 /
///   CodeMap B10 边界：HoldPose 0 改）。
/// - `scripted_start_pose`（v0.5 候选 E 段(5) 加，Spec D3 第 1 条）：
///   **仅** `submit` Scripted 路径由 coordinator 从常驻 state cache
///   （[`crate::state_cache`]）取"提交时当前位"填 `Some`；其余路径
///   （cancel / tick / 非 Scripted submit）恒 `None`。Scripted `start()`
///   缺此值 → `ActionStartError::Rejected`，**绝不** fallback 到
///   `anchor_bias`（静默跳变危险）。与 `anchor_bias` 物理隔离：两者
///   同时存在但语义不同——前者是 per-action 起点，后者是启动锚点。
/// - `command_seq`（B3 (1/2) 加，详
///   [`docs/handoffs/v0.3_b3_outline.md`](../../../docs/handoffs/v0.3_b3_outline.md)
///   §3.4 reviewer round 2 显式放行）：本 tick 即将 publish 的
///   `JointCommand` frame 的 `source_task_id` / `FrameHeader.seq`，由
///   [`crate::coordinator::run_action_tick_loop`] 维护
///   [`crate::coordinator::TickState::cmd_seq`] 透传到此。Backend
///   产 body bytes 时（如 `build_hold_command(..., command_seq, ...)`）
///   **必须**用本字段作 `source_task_id` 参数 —— S0
///   `ipc_thread::join_command_to_buf` 把 `fb.source_task_id()` 写
///   成 `JointCommandBuf.seq`，`adapter.rs` 注释要求 `seq` 单调递
///   增（`rt_thread` 判 `CONTROL_LOST` → `ACTIVE` 恢复用）；如 backend
///   私有计数从 0 起会让 S0 看到 seq 回退，破坏 wire 协议契约。**不**
///   是 audit 字段，是契约字段。
pub struct ActionContext<'a> {
    pub clock: &'a dyn RobotClock,
    pub layout_id: u64,
    pub hardware: &'a HardwareSection,
    pub config: &'a CoordinatorConfig,
    pub anchor_bias: &'a [f64],
    /// Scripted segment0 `from` = 提交时当前位（v0.5 候选 E 段(5)，Spec
    /// D3 第 1 条）。仅 `Coordinator::submit` 的 `ActionKind::Scripted`
    /// 路径填 `Some`（来源 = `StateCacheHandle::latest_fresh`，校验
    /// joint_count + layout_id + 新鲜度）；cancel / tick_action / 其它
    /// kind 恒 `None`。长度 == `hardware.joints.len()`（cache 已校验；
    /// backend 再防御断言）。
    pub scripted_start_pose: Option<&'a [f64]>,
    pub command_seq: u64,
    /// IK solver borrow（v0.4 候选 2 round 2 (3/4) 加；Q-IK3 (a) ACK
    /// `Option<&dyn ArmIkSolver>` 引用 plumb，**不** `Arc` / **不** clone）。
    ///
    /// **存在性**：`Some(_)` 仅当 `backend-ik` feature 启用 + `s1_daemon`
    /// 启动期成功构造 [`robot_kinematics_xlerobot::XLeRobotArmIkSolver`]
    /// 并注入 [`crate::coordinator::Coordinator::new`]。Coordinator 借用
    /// 内部 `Option<Box<dyn ArmIkSolver>>` 透传到本字段。
    ///
    /// **None 路径**：
    ///
    /// - `backend-ik` feature off → `s1_daemon` 永不构造 solver，注入
    ///   `None`；本字段恒为 `None`（trait crate 永远 link，类型仍存在）。
    /// - feature on 但 profile 未启用 `"ik"` → coordinator submit 永远
    ///   走 [`crate::action::SubmitError::NoBackendForKind`] 早拦掉，本
    ///   字段是否 `Some` 不可见。
    /// - feature on + profile 启用 `"ik"` + `s1_daemon` 漏注入 →
    ///   [`crate::coordinator::validate_solver_for_profile`] 在 startup
    ///   期 fail-fast（Q-IK10b (i) ACK；详 coordinator 同名函数 doc）。
    ///
    /// **lifetime `'a`**：跟 [`Self::hardware`] / [`Self::config`] /
    /// [`Self::anchor_bias`] 同一 borrow window；coordinator 持锁内构
    /// 造 `ActionContext` 时短暂借出，tick 结束即归还。`dyn ArmIkSolver`
    /// 内部 `Send + Sync` 保证跨线程访问安全（B4 RR server 多线程场景）。
    pub solver: Option<&'a dyn ArmIkSolver>,
}

// ============================================================================
// ActionTickOutput / ActionOutcome / ActionFailure
// ============================================================================

/// Backend `tick()` 单帧返回值。
#[derive(Debug, Clone)]
pub enum ActionTickOutput {
    /// 还在跑；可选附本 tick 输出的 `JointCommand` body bytes。
    ///
    /// **command 语义**（reviewer 阻断 2 锁定 / 与 outline §6 共存策略 =
    /// override 一致）：
    ///
    /// - `Some(body)`：本 tick coordinator publish 这帧 `JointCommand`；
    ///   S0 的"上次 cmd 直到 deadline"窗口被刷新。Backend 想要持续刷新
    ///   hold（防 S0 deadline 触发 control-lost）必须周期性返
    ///   `Some(...)`。
    /// - `None`：本 tick coordinator **不** publish；S0 沿用**上一帧
    ///   cmd**直到 [`ActionRequest::deadline_ns`] / S0 自己的 command
    ///   timeout 触发。**不**走 coordinator 的 hold loop —— EXECUTING
    ///   状态下 hold loop 已停发（override 策略）；要恢复 hold loop 必
    ///   须返 [`ActionTickOutput::Completed`] 让 coordinator 回 IDLE。
    Continue {
        /// `[0.0, 1.0]` 进度；不参与控制逻辑，仅给 audit / RPC stream。
        progress: f32,
        /// 本 tick 是否产生新 cmd；语义见上方 variant 文档。
        command: Option<JointCommandBody>,
    },
    /// 完成；coordinator 转回 IDLE，恢复 hold loop。
    Completed(ActionOutcome),
}

/// Action 完成时的具体结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionOutcome {
    Success,
    /// Backend 完成了大部分但有警告（例：trajectory 被 limit 削减）。
    PartialSuccess {
        warning: String,
    },
}

/// Action 失败原因。变体决定 coordinator 是回 IDLE（自恢复 hold）还是
/// 进 DEGRADED（拒绝新 action 等 `clear_error`）：
///
/// - `PreconditionFailed` / `DeadlineExceeded` → IDLE（hold 自动恢复）
/// - `SafetyAbort` / `AdapterError` / `InternalError` → DEGRADED
///
/// 详 outline §6.5 fallback 表 + §10 决策 #10 拍板方案 (d)。
#[derive(Debug, Clone, Error)]
pub enum ActionFailure {
    #[error("deadline exceeded")]
    DeadlineExceeded,
    #[error("precondition failed: {0}")]
    PreconditionFailed(String),
    #[error("adapter error: {0}")]
    AdapterError(String),
    #[error("safety abort: {0}")]
    SafetyAbort(String),
    #[error("internal error: {0}")]
    InternalError(String),
}

// ============================================================================
// ActionBackend / ActionInstance trait
// ============================================================================

/// Backend 工厂 trait：声明能力 + 创建 instance。
///
/// B2 (2/3) 时通过 `inventory::submit!` 静态注册到 `BackendRegistry`；
/// `[s1.profile.backends = [...]]` runtime enable list 决定哪些注册的
/// backend 真在 coordinator 启动时被实例化。
pub trait ActionBackend: Send + Sync {
    /// Backend 名字（跟 `inventory` 注册名一致；profile
    /// `[s1.profile.backends]` 选）。
    fn name(&self) -> &str;

    /// 本 backend 支持的 [`ActionKind`] 集合。Coordinator dispatch
    /// 时按 `(kind, schema_id)` 在所有 enabled backend 中找匹配。
    fn supported_kinds(&self) -> &[ActionKind];

    /// 本 backend 接受的 `payload_schema_id` 集合（reviewer 阻断 1 锁定，
    /// 不做空切片魔法）。
    ///
    /// **约定**：
    ///
    /// - 空 payload backend（如 `MockDwell` / `HoldPose`）必须显式返
    ///   `&[NO_PAYLOAD_SCHEMA_ID]`；coordinator dispatch 按
    ///   `(kind, schema_id)` 精确匹配，**不**对空切片做隐式 "any payload"
    ///   处理。
    /// - 返回空切片 = 该 backend 不接受任何 schema_id（dispatcher 永远
    ///   找不到匹配，等价于没注册）；仅作退化态保留，不应在生产 backend
    ///   出现。
    /// - Backend 同时支持有 / 无 payload 两条路径时，把
    ///   `NO_PAYLOAD_SCHEMA_ID` 跟具体 schema_id 一起列出（如
    ///   `&[NO_PAYLOAD_SCHEMA_ID, 0xCAFE_0001]`）。
    fn supported_schemas(&self) -> &[u32];

    /// 创建 action instance；coordinator 之后调
    /// [`ActionInstance::tick`] / [`ActionInstance::cancel`]。
    fn start(
        &self,
        req: &ActionRequest,
        ctx: &ActionContext<'_>,
    ) -> Result<Box<dyn ActionInstance>, ActionStartError>;
}

/// Backend 单次 action 执行的 instance。
///
/// **lifecycle**：`backend.start(req, ctx) → Box<dyn ActionInstance>`
/// → coordinator 50 Hz 调 `tick()` → 收到 `Completed` 或 `Err` 后 drop
/// instance。`cancel()` 是 cooperative —— set flag，下一次 `tick()`
/// 自查 + 收敛返 `Completed(PartialSuccess)` 或 `Err(...)`（§10 决策
/// #4）。
pub trait ActionInstance: Send {
    /// 50 Hz tick；backend 决定本 tick 是否产生新 `JointCommand`。
    ///
    /// 返 `Err(ActionFailure)` 立刻终止 action；coordinator 按
    /// failure 类型决定回 IDLE 还是进 DEGRADED（详 outline §6.5）。
    fn tick(&mut self, ctx: &mut ActionContext<'_>) -> Result<ActionTickOutput, ActionFailure>;

    /// Cooperative cancel：set flag，下一次 `tick()` 自查 + 收敛。
    /// 返 `Err` 仅当 backend 内部 cancel 路径出错（极少；典型实现总
    /// 返 `Ok`）。
    fn cancel(&mut self, ctx: &mut ActionContext<'_>) -> Result<(), ActionCancelError>;

    /// 终态数据通路（v0.5 候选 E 段(5)，Spec D3 第 3 条 + QA2 拍死）：
    /// action 以 `Completed(Success)` 收敛时，coordinator 调本方法决定
    /// Idle hold target——`Some(t)` → `idle_hold_target = t`；`None`
    /// → 显式 reset `idle_hold_target = 启动 anchor_bias`。
    ///
    /// **契约**：
    ///
    /// - 默认 `None`（既有 backend 0 改：HoldPose / MockDwell / IK 全部
    ///   天然 reset，无需另存 kind——穷尽式单一规则由类型承担）。
    /// - **仅** `ScriptedInstance` override：`final_published == true`
    ///   （末 waypoint target 已作为一帧 `Continue { command: Some }`
    ///   交给 coordinator publish）时返 `Some(末 waypoint target)`。
    /// - coordinator **只在** `outcome == Success ∧ !pending_cancel`
    ///   时调用；cancel / failure / PartialSuccess 路径不调、直接 reset。
    /// - 返 `Some` 的长度必须 == `hardware.joints.len()`（backend 自保）。
    fn successful_hold_target(&self) -> Option<Vec<f64>> {
        None
    }
}

// ============================================================================
// 错误枚举
// ============================================================================

/// `Coordinator::submit` 失败原因（B2 (2/3) 落地 coordinator 端时
/// 引入的具体路径来源）。
#[derive(Debug, Error)]
pub enum SubmitError {
    /// [`ActionRequest::validate`] 字段校验失败（deadline /
    /// idempotency_key / payload 长度）。
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// `[s1.profile.backends]` 没有任何 enabled backend 声明
    /// 支持本 `ActionKind`。
    #[error("no backend registered for kind {0:?}")]
    NoBackendForKind(ActionKind),
    /// 找到 backend 但其 `supported_schemas()` 不含 `payload_schema_id`。
    #[error("backend {0:?} does not support payload schema_id {1:#x}")]
    UnsupportedSchema(String, u32),
    /// B2 单 action 序列化（§10 决策 #3）：已有一个 `EXECUTING`，拒新
    /// `submit`。
    #[error("coordinator already executing action {0} (B2 single-action serialization)")]
    BusyExecuting(ActionId),
    /// Coordinator 在 DEGRADED 状态；调 `clear_error()` 才回 IDLE。
    #[error("coordinator is in DEGRADED state; clear_error required")]
    CoordinatorDegraded,
    /// 收到 SIGINT / SIGTERM 后；coordinator 不接收新 submit。
    #[error("coordinator is shutting down")]
    ShuttingDown,
    /// **reviewer 拍板补充**：`Booting` 状态拒 submit 用 `NotReady`，
    /// **不**复用 `ShuttingDown`（两阶段生命周期相反，复用会让 B4 RPC
    /// 错误码映射和 audit 日志难看）。Display 文案由 reviewer 给定，
    /// 照搬不改：`coordinator is not ready; startup anchor has not completed`。
    #[error("coordinator is not ready; startup anchor has not completed")]
    NotReady,
    /// **B2 (2/3) 实现期发现的扩展**：dispatch 通过、状态可接受，但
    /// [`ActionBackend::start`] 自身返 [`ActionStartError`] —— 比如
    /// payload decode 失败 / hardware mismatch / backend 自报 precondition
    /// 拒绝。`SubmitError` 既有变体不覆盖这条路径，纯加 variant 桥接
    /// `submit` 返回类型，不动既有变体语义（reviewer 拍板 "如确实需要补
    /// ActionTicket/ActionEvent/ActionStatus 直接相关 action API 类型"
    /// 范围内）。
    #[error("backend start failed: {0}")]
    BackendStartFailed(#[from] ActionStartError),
}

/// `ActionBackend::start` 失败原因。**变体非穷举**（B3 起 backend 自
/// 己定义具体拒绝原因；B2 (1/3) 仅声明通用三类）。
#[derive(Debug, Error)]
pub enum ActionStartError {
    /// Backend 显式拒绝（precondition / hardware mismatch / etc）。
    #[error("backend rejected request: {0}")]
    Rejected(String),
    /// `payload` 字节流按 `payload_schema_id` 解码失败。
    #[error("payload decode failed: {0}")]
    PayloadDecode(String),
    /// `ActionContext.hardware` 跟 backend 期望不匹配（joint count /
    /// adapter capability 等）。
    #[error("hardware mismatch: {0}")]
    HardwareMismatch(String),
}

/// `ActionInstance::cancel` 失败原因（B2 (1/3) 仅声明；典型实现总返
/// `Ok` —— cooperative cancel 是 set flag，几乎不会失败）。
#[derive(Debug, Error)]
pub enum ActionCancelError {
    #[error("backend cancel hook returned error: {0}")]
    BackendError(String),
    #[error("action not currently executing")]
    NotExecuting,
}

/// `Coordinator::clear_error()` 失败原因（B2 (2/3) stub 内部 API；B4
/// 接 RPC `ClearError`）。
#[derive(Debug, Error)]
pub enum ClearError {
    /// 当前不在 DEGRADED 状态；`clear_error()` 仅 DEGRADED → IDLE
    /// 转移路径有效。
    #[error("coordinator not in DEGRADED state (current: {0})")]
    NotDegraded(String),
    /// 极少；保留兜底（state lock poison / 等）。
    #[error("internal error: {0}")]
    InternalError(String),
}

// ============================================================================
// ActionTicket / ActionEvent / ActionStatus（B2 (2/3) Coordinator API 表层）
// ============================================================================

/// 一次 [`crate::coordinator`] 端 `submit()` 成功后返回的执行凭证。
///
/// **持有方典型路径**：
///
/// 1. 调 `submit(req) → Ok(ticket)`，保留 `ticket.action_id` 跟踪状态。
/// 2. 调 `ticket.events.try_recv()` / `recv()` 拉事件流（B2 内 in-process
///    同步 channel；B4 时 robot-rpc-gateway 起 adapter thread 桥到 tonic
///    `StreamTaskEvents`，sync ↔ async 边界**不**进 robot-s1）。
/// 3. terminal 事件（[`ActionEvent::Completed`] / [`ActionEvent::Failed`]
///    / [`ActionEvent::Cancelled`]）后 sender drop，`recv()` 返
///    `Err(RecvError)` = 事件流关闭。
///
/// **不**实现 [`Clone`]：`std::sync::mpsc::Receiver` 是 single-consumer，
/// 复制 ticket 会让事件流半边丢；ticket 持有方应**唯一**。`Debug` 因
/// `Receiver<T>` 不实现 `Debug`，手写以保 `Coordinator` API 周围日志友好。
///
/// **ticket drop 行为**：reviewer 阻断锁定 —— 持有方提前 drop ticket 导致
/// coordinator 内部 `events_tx.send(...)` 返 `Err(SendError)` 时，coordinator
/// **不**把它当 action failure；记一行 debug log 后 swallow，状态机继续按
/// backend tick 输出推进。
pub struct ActionTicket {
    pub action_id: ActionId,
    /// 同步事件 channel（reviewer 阻断 1 锁定：**不**用 tokio）。
    pub events: std::sync::mpsc::Receiver<ActionEvent>,
}

impl core::fmt::Debug for ActionTicket {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ActionTicket")
            .field("action_id", &self.action_id)
            .field("events", &"<Receiver<ActionEvent>>")
            .finish()
    }
}

/// Coordinator 推到 [`ActionTicket::events`] 的事件流。
///
/// **保序**：单 ticket 内事件按时间顺序到达；**不**跨 ticket 保序。
/// **终止**：每个 ticket 至多收到**一条**终态事件
/// （[`ActionEvent::Completed`] / [`ActionEvent::Failed`] /
/// [`ActionEvent::Cancelled`]）；之后 sender drop。reviewer 拍板锁定：
/// 取消后正常收敛 = `Cancelled`；取消后 backend 报错 = `Failed`；
/// **绝不**双发 `Completed + Cancelled`。
///
/// `at_ns` 全部用 `u64`（与 [`ActionRequest::deadline_ns`] 对齐，不裹
/// `MonoRawNs` newtype）。
#[derive(Debug, Clone)]
pub enum ActionEvent {
    /// Backend `start()` 成功 + 状态转 EXECUTING；`at_ns` = 转移时刻。
    Started { at_ns: u64 },
    /// Backend tick 返 [`ActionTickOutput::Continue`]；coordinator 按
    /// `progress` 变化推（B2 mock：每次 tick 都推；B3+ scheduler 优化
    /// 为 progress delta > threshold 才推）。
    Progress { progress: f32, at_ns: u64 },
    /// Backend tick 返 [`ActionTickOutput::Completed`]；终态事件。
    Completed { outcome: ActionOutcome, at_ns: u64 },
    /// Backend tick 返 `Err(failure)` 或 cancel 后 backend 报错；终态事件。
    Failed { failure: ActionFailure, at_ns: u64 },
    /// `Coordinator::cancel()` 调用 + backend cancel 路径正常收敛后；
    /// 终态事件（不会跟 `Completed` 同时发）。
    Cancelled { at_ns: u64 },
}

/// `Coordinator::status(action_id)` 查询返值。
///
/// **B2 单 action 序列化**：`Pending` 在 B2 **不构造**（`submit` 即转
/// EXECUTING，无 queue 缓冲）；保留 variant 是为对齐 B3 scheduler 引入
/// queue 后的语义，让 RPC `GetTaskStatus` 字段集 stable。
///
/// **finished ring**（reviewer 拍板）：terminal 事件发出后 coordinator
/// 把 `(ActionId, ActionStatus)` 写入 cap=128 的 `VecDeque` ring buffer；
/// `status(id)` 先查 EXECUTING 再查 ring，找不到才返 `None`。超 128 淘汰
/// 最旧。**不**做无界 map（防 long-running daemon 慢漏内存）。
#[derive(Debug, Clone)]
pub enum ActionStatus {
    /// 已接收但 backend 未 start；**B2 不构造**（B3 scheduler queue 时进入）。
    Pending,
    Executing {
        progress: f32,
    },
    Completed(ActionOutcome),
    Failed(ActionFailure),
    Cancelled,
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ----- ActionId -----

    #[test]
    fn action_id_new_v7_returns_uuid_version_7() {
        let id = ActionId::new_v7();
        // UUIDv7：version field = 7（高 4 bit of byte 6）。
        assert_eq!(id.0.get_version_num(), 7);
    }

    #[test]
    fn action_id_two_v7_calls_are_distinct() {
        let a = ActionId::new_v7();
        let b = ActionId::new_v7();
        assert_ne!(a, b);
    }

    #[test]
    fn action_id_display_matches_uuid() {
        let raw = Uuid::now_v7();
        let id = ActionId(raw);
        assert_eq!(format!("{id}"), raw.to_string());
    }

    // ----- ActionKind -----

    #[test]
    fn action_kind_four_v04_variants() {
        // 锁住"v0.4 候选 2 round 2 (2/4) land 后 enum = DwellMs / HoldPose /
        // Scripted / Ik" 边界 —— 任何加新 variant 的实现 commit 必须先
        // 回到 outline reviewer 复核。用 match 穷举 + 编译期检查：如果
        // 有人加新 variant，下面 exhaustive 的 unit test 会编不过。
        fn cover(k: ActionKind) -> u8 {
            match k {
                ActionKind::DwellMs => 0,
                ActionKind::HoldPose => 1,
                ActionKind::Scripted => 2,
                ActionKind::Ik => 3,
            }
        }
        assert_eq!(cover(ActionKind::DwellMs), 0);
        assert_eq!(cover(ActionKind::HoldPose), 1);
        assert_eq!(cover(ActionKind::Scripted), 2);
        assert_eq!(cover(ActionKind::Ik), 3);
    }

    #[test]
    fn action_kind_eq_and_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(ActionKind::DwellMs);
        set.insert(ActionKind::HoldPose);
        set.insert(ActionKind::Scripted);
        set.insert(ActionKind::DwellMs); // dup
        assert_eq!(set.len(), 3);
        assert!(set.contains(&ActionKind::DwellMs));
        assert!(set.contains(&ActionKind::HoldPose));
        assert!(set.contains(&ActionKind::Scripted));
    }

    // ----- JointCommandBody -----

    #[test]
    fn joint_command_body_round_trip() {
        let bytes = vec![1u8, 2, 3, 4];
        let body = JointCommandBody::new(bytes.clone());
        assert_eq!(body.as_bytes(), &bytes[..]);
        assert_eq!(body.len(), 4);
        assert!(!body.is_empty());
        assert_eq!(body.into_bytes(), bytes);
    }

    #[test]
    fn joint_command_body_empty() {
        let body = JointCommandBody::new(Vec::new());
        assert!(body.is_empty());
        assert_eq!(body.len(), 0);
        assert_eq!(body.as_bytes(), &[] as &[u8]);
    }

    // ----- ActionRequest::validate -----

    fn sample_request(now_ns: u64) -> ActionRequest {
        ActionRequest {
            action_id: ActionId::new_v7(),
            kind: ActionKind::DwellMs,
            deadline_ns: now_ns + 500_000_000, // +500 ms
            priority: 10,
            payload_schema_id: NO_PAYLOAD_SCHEMA_ID,
            payload: Vec::new(),
            idempotency_key: "client-retry-key-001".into(),
            source: ActionSource::Internal,
        }
    }

    #[test]
    fn validate_happy_path() {
        let now_ns: u64 = 1_000_000_000;
        let req = sample_request(now_ns);
        req.validate(now_ns).expect("should pass");
    }

    #[test]
    fn validate_rejects_deadline_eq_now() {
        let now_ns: u64 = 1_000_000_000;
        let mut req = sample_request(now_ns);
        req.deadline_ns = now_ns;
        let err = req.validate(now_ns).unwrap_err();
        assert!(
            matches!(err, SubmitError::InvalidRequest(ref s) if s.contains("deadline_ns")),
            "got {err:?}"
        );
    }

    #[test]
    fn validate_rejects_deadline_before_now() {
        let now_ns: u64 = 1_000_000_000;
        let mut req = sample_request(now_ns);
        req.deadline_ns = now_ns - 1;
        let err = req.validate(now_ns).unwrap_err();
        assert!(
            matches!(err, SubmitError::InvalidRequest(ref s) if s.contains("deadline_ns")),
            "got {err:?}"
        );
    }

    #[test]
    fn validate_rejects_empty_idempotency_key() {
        let now_ns: u64 = 1_000_000_000;
        let mut req = sample_request(now_ns);
        req.idempotency_key.clear();
        let err = req.validate(now_ns).unwrap_err();
        assert!(
            matches!(err, SubmitError::InvalidRequest(ref s) if s.contains("idempotency_key")),
            "got {err:?}"
        );
    }

    #[test]
    fn validate_rejects_oversize_idempotency_key() {
        let now_ns: u64 = 1_000_000_000;
        let mut req = sample_request(now_ns);
        req.idempotency_key = "x".repeat(MAX_IDEMPOTENCY_KEY_LEN + 1);
        let err = req.validate(now_ns).unwrap_err();
        assert!(
            matches!(err, SubmitError::InvalidRequest(ref s)
                if s.contains("idempotency_key") && s.contains("MAX_IDEMPOTENCY_KEY_LEN")),
            "got {err:?}"
        );
    }

    #[test]
    fn validate_rejects_oversize_payload() {
        let now_ns: u64 = 1_000_000_000;
        let mut req = sample_request(now_ns);
        req.payload = vec![0u8; MAX_PAYLOAD_LEN + 1];
        let err = req.validate(now_ns).unwrap_err();
        assert!(
            matches!(err, SubmitError::InvalidRequest(ref s)
                if s.contains("payload") && s.contains("MAX_PAYLOAD_LEN")),
            "got {err:?}"
        );
    }

    #[test]
    fn validate_accepts_payload_at_max_len() {
        let now_ns: u64 = 1_000_000_000;
        let mut req = sample_request(now_ns);
        // 显式给非哨兵 schema_id —— 新 reviewer 规则要求
        // `schema_id == NO_PAYLOAD_SCHEMA_ID` 时 payload 必须为空。
        req.payload_schema_id = 0xCAFE_0001;
        req.payload = vec![0u8; MAX_PAYLOAD_LEN];
        req.validate(now_ns).expect("len == MAX must pass");
    }

    #[test]
    fn validate_accepts_idempotency_key_at_max_len() {
        let now_ns: u64 = 1_000_000_000;
        let mut req = sample_request(now_ns);
        req.idempotency_key = "x".repeat(MAX_IDEMPOTENCY_KEY_LEN);
        req.validate(now_ns).expect("len == MAX must pass");
    }

    #[test]
    fn validate_rejects_no_payload_schema_id_with_non_empty_payload() {
        // reviewer 阻断 1 锁定：客户端不能用哨兵 schema_id 偷渡 bytes。
        let now_ns: u64 = 1_000_000_000;
        let mut req = sample_request(now_ns);
        assert_eq!(req.payload_schema_id, NO_PAYLOAD_SCHEMA_ID);
        req.payload = vec![1, 2, 3];
        let err = req.validate(now_ns).unwrap_err();
        assert!(
            matches!(err, SubmitError::InvalidRequest(ref s)
                if s.contains("NO_PAYLOAD_SCHEMA_ID") && s.contains("empty payload")),
            "got {err:?}"
        );
    }

    #[test]
    fn validate_accepts_no_payload_schema_id_with_empty_payload() {
        // 哨兵 schema_id + 空 payload 是 MockDwell / HoldPose 的合法路径。
        let now_ns: u64 = 1_000_000_000;
        let req = sample_request(now_ns);
        assert_eq!(req.payload_schema_id, NO_PAYLOAD_SCHEMA_ID);
        assert!(req.payload.is_empty());
        req.validate(now_ns).expect("must pass");
    }

    #[test]
    fn validate_accepts_non_sentinel_schema_id_with_non_empty_payload() {
        // 真 backend（B3 起 trajectory / policy chunk）的合法路径。
        let now_ns: u64 = 1_000_000_000;
        let mut req = sample_request(now_ns);
        req.payload_schema_id = 0xCAFE_0001;
        req.payload = vec![0xDE, 0xAD, 0xBE, 0xEF];
        req.validate(now_ns).expect("must pass");
    }

    #[test]
    fn no_payload_schema_id_is_zero() {
        // 锁住 "默认 / unset = 哨兵" 跟 RPC `CommandMeta.payload_schema_id`
        // 字段未设置默认值的对齐关系；改这个常量等于改 wire 协议。
        assert_eq!(NO_PAYLOAD_SCHEMA_ID, 0);
    }

    // ----- ActionOutcome / ActionFailure / SubmitError Display -----

    #[test]
    fn action_outcome_equality() {
        assert_eq!(ActionOutcome::Success, ActionOutcome::Success);
        let warn = ActionOutcome::PartialSuccess {
            warning: "trajectory clamped".into(),
        };
        let warn2 = ActionOutcome::PartialSuccess {
            warning: "trajectory clamped".into(),
        };
        assert_eq!(warn, warn2);
        assert_ne!(warn, ActionOutcome::Success);
    }

    #[test]
    fn action_failure_display_covers_all_variants() {
        let cases: &[(ActionFailure, &str)] = &[
            (ActionFailure::DeadlineExceeded, "deadline exceeded"),
            (
                ActionFailure::PreconditionFailed("limits".into()),
                "precondition failed: limits",
            ),
            (
                ActionFailure::AdapterError("serial timeout".into()),
                "adapter error: serial timeout",
            ),
            (
                ActionFailure::SafetyAbort("estop fired".into()),
                "safety abort: estop fired",
            ),
            (
                ActionFailure::InternalError("oops".into()),
                "internal error: oops",
            ),
        ];
        for (failure, expected) in cases {
            assert_eq!(failure.to_string(), *expected);
        }
    }

    #[test]
    fn submit_error_display_main_variants() {
        let cases: &[(SubmitError, &str)] = &[
            (
                SubmitError::InvalidRequest("foo".into()),
                "invalid request: foo",
            ),
            (
                SubmitError::NoBackendForKind(ActionKind::DwellMs),
                "no backend registered for kind DwellMs",
            ),
            (
                SubmitError::CoordinatorDegraded,
                "coordinator is in DEGRADED state; clear_error required",
            ),
            (SubmitError::ShuttingDown, "coordinator is shutting down"),
            // reviewer 拍板补充：Booting 拒 submit 用 NotReady（不复用
            // ShuttingDown）；Display 文案逐字符照搬。
            (
                SubmitError::NotReady,
                "coordinator is not ready; startup anchor has not completed",
            ),
            // B2 (2/3) 实现期扩展：backend.start 透传 ActionStartError。
            (
                SubmitError::BackendStartFailed(ActionStartError::Rejected("usb timeout".into())),
                "backend start failed: backend rejected request: usb timeout",
            ),
        ];
        for (err, expected) in cases {
            assert_eq!(err.to_string(), *expected);
        }
    }

    #[test]
    fn submit_error_unsupported_schema_renders_hex() {
        let err = SubmitError::UnsupportedSchema("rl_onnx".into(), 0xABCD_0001);
        assert_eq!(
            err.to_string(),
            "backend \"rl_onnx\" does not support payload schema_id 0xabcd0001"
        );
    }

    #[test]
    fn submit_error_busy_executing_renders_uuid() {
        let id = ActionId::new_v7();
        let err = SubmitError::BusyExecuting(id);
        let s = err.to_string();
        assert!(
            s.contains(&id.to_string()),
            "expected UUID in display, got {s}"
        );
        assert!(s.contains("single-action"));
    }

    // ----- ActionTicket / ActionEvent / ActionStatus -----

    #[test]
    fn action_ticket_holds_action_id_and_receiver() {
        let id = ActionId::new_v7();
        let (tx, rx) = std::sync::mpsc::channel::<ActionEvent>();
        let ticket = ActionTicket {
            action_id: id,
            events: rx,
        };
        assert_eq!(ticket.action_id, id);
        // Receiver 端 try_recv 空 channel 返 Empty。
        assert!(matches!(
            ticket.events.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        // Sender 端 drop 后 try_recv 返 Disconnected。
        drop(tx);
        assert!(matches!(
            ticket.events.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn action_ticket_debug_does_not_leak_receiver_internals() {
        let id = ActionId::new_v7();
        let (_tx, rx) = std::sync::mpsc::channel::<ActionEvent>();
        let ticket = ActionTicket {
            action_id: id,
            events: rx,
        };
        let s = format!("{ticket:?}");
        assert!(s.contains("ActionTicket"));
        assert!(s.contains(&id.to_string()));
        assert!(s.contains("<Receiver<ActionEvent>>"));
    }

    #[test]
    fn action_event_round_trip_through_channel() {
        // 锁住"发 / 收 / Clone"路径都通；ActionFailure / ActionOutcome 已 Clone。
        let (tx, rx) = std::sync::mpsc::channel::<ActionEvent>();
        let evs = [
            ActionEvent::Started { at_ns: 1 },
            ActionEvent::Progress {
                progress: 0.5,
                at_ns: 2,
            },
            ActionEvent::Cancelled { at_ns: 3 },
        ];
        for e in &evs {
            tx.send(e.clone()).expect("send");
        }
        for expected in &evs {
            let got = rx.recv().expect("recv");
            // Debug + Clone 路径都 round trip。
            assert_eq!(format!("{got:?}"), format!("{expected:?}"));
        }
    }

    #[test]
    fn action_event_terminal_variants_carry_payload() {
        let completed = ActionEvent::Completed {
            outcome: ActionOutcome::Success,
            at_ns: 100,
        };
        match completed {
            ActionEvent::Completed { outcome, at_ns } => {
                assert_eq!(outcome, ActionOutcome::Success);
                assert_eq!(at_ns, 100);
            }
            _ => panic!("variant mismatch"),
        }
        let failed = ActionEvent::Failed {
            failure: ActionFailure::DeadlineExceeded,
            at_ns: 200,
        };
        match failed {
            ActionEvent::Failed { failure, at_ns } => {
                assert!(matches!(failure, ActionFailure::DeadlineExceeded));
                assert_eq!(at_ns, 200);
            }
            _ => panic!("variant mismatch"),
        }
    }

    #[test]
    fn action_status_five_variants_clone_and_debug() {
        // 锁住 5 variant 形状 + Clone + Debug；Pending 在 B2 不构造但
        // 类型上必须可用（B3 scheduler 用）。
        let cases = [
            ActionStatus::Pending,
            ActionStatus::Executing { progress: 0.25 },
            ActionStatus::Completed(ActionOutcome::Success),
            ActionStatus::Failed(ActionFailure::AdapterError("usb timeout".into())),
            ActionStatus::Cancelled,
        ];
        for s in &cases {
            let cloned = s.clone();
            assert_eq!(format!("{s:?}"), format!("{cloned:?}"));
        }
    }
}
