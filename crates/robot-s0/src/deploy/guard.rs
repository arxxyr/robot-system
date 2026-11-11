//! S0 部署主进程：校验本侧 arm-only 或显式完整总线身份，再执行原六步。
//!
//! ## 为什么 guard 必须是主进程（QA1 / Y1 终拍）
//!
//! `RestartPreventExitStatus=86` **只作用于主进程**。若 pre 步放
//! `ExecStartPre=`，退 86 不会被捕获 → 连败停止态失效。故 unit 写
//! `ExecStart=robot-deploy guard …`，guard 末尾 `exec robot-s0`
//! 保住 `MainPID`（`Type=notify` / watchdog 语义照旧）。
//!
//! ## 六步（pre-increment，r11 AB1）
//!
//! 1. 持稳定 `.lock`，读 `n`；`n == 5` → 立即退 86；
//! 2. **执行任何 preflight / provision 之前**原子写入 `n+1`（写失败 → 86）；
//! 3. 两步全部成功 → **清零计数**（清零失败退 86）→ `exec robot-s0`；
//! 4. 正常失败：当前值 `== 5` → 退 86；不足 5 → 普通非零码（走 unit
//!    `Restart=on-failure` + `RestartSec=2s` 重试）；
//! 5. 同一 boot 内的进程终止（panic / SIGKILL）**保留已递增值**，下一次
//!    启动从 1 继续累计；
//! 6. 计数语义 = **未成功 guard 尝试数**（非"已完成失败数"）。
//!
//! ## 调用前置校验（exec 前，锁之前）
//!
//! `s0_bin` 必须 `is_absolute()` + `exists()`；`--` 后的 s0 args **不得**
//! 自带 `--config`（R10-1 单源规则：guard 自动追加
//! `--config <GuardConfig.config_path>`）。两者都是**配置级、不可自动恢复**
//! 的错误，与 [`GuardExit::ExecFailed`] 的 `ENOENT`/`EACCES` 同族 → 退 86。
//! 放在取锁**之前**：这类错误每次重试必然重现，不该消耗连败预算，且
//! 86 本身已停掉自动重启。
//!
//! ## 身份绑定
//!
//! `--arm` 决定**安全写序**，config 决定**物理 bus**，s0 argv 决定 daemon
//! 的**运行身份**——三者必须选择同一侧、同一种配置，否则会操作错 bus。
//! 故 guard 在**取锁之前**做两级校验：
//!
//! 1. **纯 argv 级**（[`validate_invocation`]）：本侧既有 robot ID 选择
//!    arm-only/full；转发的 robot/hardware ID 必须一致，禁止重复 config；
//! 2. **config 文件级**（[`GuardSteps::verify_identity`]）：复用身份、布局、
//!    标定与相机校验，并核对实际 config 与 argv 的运行身份相同。
//!
//! 两级都是**配置级、不可自动恢复**错误 → 退 86，且**不消耗连败预算**。
//!
//! ## latch 稳定锁的类型化分流（S48 A3d / W1）
//!
//! provision 内部持目标稳定锁 `<latch>.lock` 覆盖整个 read / repair / fsync，
//! 返回即释放（RAII），因此 exec 前 guard 不持有它，`robot-s0` 自行重取。
//! 锁被另一进程持有 → [`GuardExit::LatchHeld`] 退 **89**（unit 禁止重启，与
//! daemon 的 89 同义）；latch 名字 / 锁 IO / 对象身份错误 →
//! [`GuardExit::LatchConfig`] 退 **1**（`Restart=on-failure` 仍会重试，连败预算
//! 照常累计到 86）。两者都**不**压成字符串落入旧 71 / 86 分支；原三分支业务
//! 失败仍走 [`PROVISION_FAIL_EXIT_CODE`] 与连败预算。
//!
//! ## latch 路径单源
//!
//! provision 的目标路径**跟随 s0 args**：`--latch-path <p>` 在场即用它，
//! arm-only 缺省时回填 `/var/lib/robot/<robot_id>/estop_latch`（与 daemon 的默认
//! 回填规则逐字一致）——保证 guard provision 的文件正是 daemon 将要读的
//! 那一份。
//! 完整总线必须显式给出 `--latch-path` 与同父目录的 `--rx-trust-dir`，
//! 不按新 robot ID 隐式新建保护目录。
//!
//! **解析规则必须与 `robot-s0::Args::parse` 同规则**（round 1 P0-3）：
//! daemon 只认 `--opt <value>` 空格形式、**不认 `--opt=value`**，且对重复
//! 取**最后一个**。首版 guard 取第一个且接受等号形式——重复或等号写法下
//! guard provision A、daemon 读 B。修法 = **guard 拒绝重复 / 缺值 / 等号
//! 写法**，只接受唯一一处空格形式（唯一即无歧义，两侧必然同值）。

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use super::counter::{
    LockError, MAX_CONSECUTIVE_FAILURES, acquire_instance_lock, count_path, lock_path, read_count,
    write_count_atomic,
};
use super::identity;
use super::provision::ProvisionError;
use super::types::DeployArm;
use crate::latch_lock::{
    LATCH_LOCK_FAILURE_EXIT_CODE, LATCH_LOCK_HELD_EXIT_CODE, validate_latch_path,
};

/// 停止态退出码——**永久保留**（`robot-s0` 任何路径不得以 86 退出，
/// 否则 `RestartPreventExitStatus=86` 会误吞 watchdog 重启；回归锁见 AC-E3）。
pub const GUARD_STOP_EXIT_CODE: u8 = 86;
/// preflight 普通失败码（走 `RestartSec=2s` 重试）。
pub const PREFLIGHT_FAIL_EXIT_CODE: u8 = 70;
/// provision 普通失败码（走 `RestartSec=2s` 重试）。
pub const PROVISION_FAIL_EXIT_CODE: u8 = 71;

/// s0 args 里禁止出现的选项（R10-1 单源规则）。
const FORBIDDEN_S0_OPT: &str = "--config";
/// latch 路径选项（单源解析，**不**禁止；必须唯一且为空格形式）。
const LATCH_PATH_OPT: &str = "--latch-path";
/// 完整总线必须显式绑定既有保护目录。
const RX_TRUST_DIR_OPT: &str = "--rx-trust-dir";
/// 运行身份选项——必须存在且与 D2 一致（round 1 P0-1）。
const ROBOT_ID_OPT: &str = "--robot-id";
/// 硬件身份选项——必须存在且与 D2 一致（round 1 P0-1）。
const HARDWARE_ID_OPT: &str = "--hardware-id";

/// guard 的完整调用参数。
///
/// `s0_args` = unit `ExecStart` 里 `--` 之后的部分（**不含** `--config`；
/// guard 自动追加，见模块 doc）。
#[derive(Debug, Clone)]
pub struct GuardConfig {
    /// 本臂（决定 preflight 触哪条 bus）。
    pub arm: DeployArm,
    /// 本臂 config 绝对路径（三处引用同一份，R10-1）。
    pub config_path: PathBuf,
    /// `robot-s0` 可执行文件——必须 `is_absolute()` + `exists()`，违者退 86。
    pub s0_bin: PathBuf,
    /// systemd `RuntimeDirectory` 注入的计数目录。
    pub runtime_dir: PathBuf,
    /// 运行身份（计数 / 实例锁按它派生；完整总线显式绑定保护路径）。
    pub robot_id: String,
    /// `--` 之后透传给 `robot-s0` 的参数。
    pub s0_args: Vec<OsString>,
}

impl GuardConfig {
    /// 本实例的 count 文件路径。
    #[must_use]
    pub fn count_file(&self) -> PathBuf {
        count_path(&self.runtime_dir, &self.robot_id)
    }

    /// 本实例的稳定锁文件路径。
    #[must_use]
    pub fn lock_file(&self) -> PathBuf {
        lock_path(&self.runtime_dir, &self.robot_id)
    }

    /// provision 目标 latch 路径——**跟随 s0 args**（见模块 doc）。
    ///
    /// # Errors
    ///
    /// `--latch-path` 重复 / 缺值 / 写成 `--latch-path=<p>` → 与
    /// `robot-s0` 的取值可能不同，一律拒绝（配置级错误 → 86）。
    pub fn resolve_latch_path(&self) -> Result<PathBuf, String> {
        match scan_opt(&self.s0_args, LATCH_PATH_OPT)? {
            Some(p) => Ok(PathBuf::from(p)),
            None if identity::expected_hardware_id(self.arm, &self.robot_id).is_some()
                && self.robot_id != identity::expected(self.arm).robot_id =>
            {
                Err("完整总线必须显式指定 --latch-path".to_string())
            }
            None => Ok(PathBuf::from(format!(
                "/var/lib/robot/{}/estop_latch",
                self.robot_id
            ))),
        }
    }
}

/// 按 **`robot-s0::Args::parse` 的同一套 tokenize 规则**取某选项的唯一取值。
///
/// - 只认 `--opt <value>`：命中即把**下一个 token 无条件当取值**（与 daemon
///   逐字一致，避免 `--latch-path --robot-id x` 这类错位两侧解释不同）；
/// - `--opt=<value>` → 拒绝（daemon 不认识这种写法，会当未知参数）；
/// - 出现 **> 1 次** → 拒绝（daemon 取最后一个、首版 guard 取第一个，重复
///   时两侧必然分叉）；
/// - 命中但无取值 → 拒绝（daemon 侧同样是错误）。
fn scan_opt(args: &[OsString], opt: &str) -> Result<Option<OsString>, String> {
    let eq_prefix = format!("{opt}=");
    let mut found: Option<OsString> = None;
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg
            .to_str()
            .is_some_and(|t| t.starts_with(eq_prefix.as_str()))
        {
            return Err(format!(
                "{opt}=<value> is not accepted by robot-s0; use `{opt} <value>`"
            ));
        }
        if arg == OsStr::new(opt) {
            let value = args
                .get(i + 1)
                .ok_or_else(|| format!("{opt} requires a value"))?;
            if found.is_some() {
                return Err(format!(
                    "{opt} appears more than once (robot-s0 takes the last, guard the first — ambiguous)"
                ));
            }
            found = Some(value.clone());
            i += 2;
            continue;
        }
        i += 1;
    }
    Ok(found)
}

/// argv 里是否出现过某选项（`--opt` 或 `--opt=…`）。
fn has_opt(args: &[OsString], opt: &str) -> bool {
    args.iter().any(|arg| {
        arg == OsStr::new(opt)
            || arg
                .to_str()
                .and_then(|t| t.strip_prefix(opt))
                .is_some_and(|rest| rest.starts_with('='))
    })
}

/// guard 的失败形态；[`GuardExit::code`] 给出进程退出码。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardExit {
    /// `exec` 返回即失败（`ENOENT`/`EACCES`），或 exec 前的**配置级**前置
    /// 校验失败（`s0_bin` 非绝对路径 / 不存在、s0 args 自带 `--config`）
    /// ——同族：配置级、不可自动恢复 → 86。
    ExecFailed,
    /// 连败预算耗尽（进入停止态，需人工恢复）。
    BudgetExhausted,
    /// 计数文件读 / 写 / 清零异常（**禁止默认归零、禁止继续 exec**）。
    CounterIo,
    /// 同实例并发（`flock` 非阻塞获取失败）。
    LockConflict,
    /// preflight / provision 正常失败，普通非零码。
    PreStepFailed(u8),
    /// W1：latch 目标稳定锁被另一进程持有 → 89（unit 禁止重启；计数保留已递增值）。
    LatchHeld,
    /// W1：latch 名字 / 锁 IO / 对象身份错误 → 1（配置 / 系统错误，不伪称被占用）。
    LatchConfig,
}

impl GuardExit {
    /// 进程退出码。
    #[must_use]
    pub const fn code(&self) -> u8 {
        match self {
            Self::ExecFailed | Self::BudgetExhausted | Self::CounterIo | Self::LockConflict => {
                GUARD_STOP_EXIT_CODE
            }
            Self::PreStepFailed(code) => *code,
            Self::LatchHeld => LATCH_LOCK_HELD_EXIT_CODE,
            Self::LatchConfig => LATCH_LOCK_FAILURE_EXIT_CODE,
        }
    }

    /// 是否进入"停止自动重启"的停止态（退 86）。
    #[must_use]
    pub const fn is_stop_state(&self) -> bool {
        self.code() == GUARD_STOP_EXIT_CODE
    }
}

/// provision 步骤的类型化失败（S48 A3d / W1）：锁被占用与配置 / 系统错误各自
/// 保持独立退出码，只有原三分支业务失败才进入 71 / 连败预算。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProvisionStepError {
    /// latch 稳定锁被另一进程持有 → [`GuardExit::LatchHeld`]（89）。
    LatchHeld { pid: Option<u32> },
    /// latch 名字 / 锁 IO / 对象身份错误 → [`GuardExit::LatchConfig`]（1）。
    LatchConfig(String),
    /// 原三分支业务失败（损坏副本 / 写入 / 目录 fsync）→ 71 或预算耗尽 86。
    Failed(String),
}

impl From<ProvisionError> for ProvisionStepError {
    fn from(error: ProvisionError) -> Self {
        match error {
            ProvisionError::LatchLockHeld { pid } => Self::LatchHeld { pid },
            ProvisionError::InvalidLatchName(_) | ProvisionError::LatchLock(_) => {
                Self::LatchConfig(error.to_string())
            }
            ProvisionError::UnusableExisting { .. }
            | ProvisionError::Write { .. }
            | ProvisionError::DirFsync { .. } => Self::Failed(error.to_string()),
        }
    }
}

/// `exec` 的结果。真实现**只可能**返回 [`ExecOutcome::Failed`]——成功即不返回。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecOutcome {
    /// 仅测试实现可返回：断言"已到达 exec 且计数已清零"。
    Execed,
    /// `exec` 返回 → 失败原因。
    Failed(String),
}

/// guard 的两个 pre 步 + exec，抽成 trait 以便 unit 级判据不碰真串口。
pub trait GuardSteps {
    /// **config 文件级** D2 身份校验（round 1 P0-1）：`arm` 与 config 必须
    /// 指向同一条 D2 行，否则会打开错 bus 却跑本侧写序。
    ///
    /// 在取锁**之前**调用，失败 = 配置级错误退 86 且不消耗连败预算。
    ///
    /// # Errors
    ///
    /// 见 [`identity::IdentityError`]。
    fn verify_identity(&mut self, cfg: &GuardConfig) -> Result<(), String>;

    /// 本臂 bus ensure-safe。`Err` = 人可读原因。
    ///
    /// # Errors
    ///
    /// 串口构造 / 安全写 / readback 任一失败。
    fn preflight(&mut self, cfg: &GuardConfig) -> Result<(), String>;

    /// 本臂 latch provision（持目标稳定锁覆盖整个过程，返回即释放）。
    ///
    /// # Errors
    ///
    /// 见 [`ProvisionStepError`]：锁被占用 / 配置错误 / 业务失败三类分流。
    fn provision(&mut self, cfg: &GuardConfig) -> Result<(), ProvisionStepError>;

    /// `exec robot-s0`（成功不返回）。
    fn exec_s0(&mut self, cfg: &GuardConfig, args: &[OsString]) -> ExecOutcome;
}

/// 拼 `robot-s0` 的完整 argv：透传 `s0_args` 后**自动追加**
/// `--config <config_path>`。
///
/// # Errors
///
/// `s0_args` 自带 `--config` → [`GuardExit::ExecFailed`]（R10-1 单源规则）。
pub fn build_s0_args(cfg: &GuardConfig) -> Result<Vec<OsString>, GuardExit> {
    if has_opt(&cfg.s0_args, FORBIDDEN_S0_OPT) {
        tracing::error!(
            robot_id = %cfg.robot_id,
            "guard: s0 args must not carry --config (guard appends it; R10-1 single source)"
        );
        return Err(GuardExit::ExecFailed);
    }
    let mut args = cfg.s0_args.clone();
    args.push(OsString::from(FORBIDDEN_S0_OPT));
    args.push(cfg.config_path.clone().into_os_string());
    Ok(args)
}

/// exec 前的**纯 argv 级**配置校验：`s0_bin` 绝对路径 + 存在、D2 身份三项
/// （`robot_id` / s0 argv `--robot-id` / `--hardware-id`）、`--latch-path`
/// 单源可解析，以及 latch **纯名称校验**（W1，见 [`validate_latch_path`]）。
///
/// 不读 config 文件——文件级校验在 [`GuardSteps::verify_identity`]。
///
/// # Errors
///
/// 见 [`GuardExit::ExecFailed`]（配置级、不可自动恢复 → 86）；latch 名字不合法
/// → [`GuardExit::LatchConfig`]（1），且不产生锁 / 计数副作用。
pub fn validate_invocation(cfg: &GuardConfig) -> Result<(), GuardExit> {
    let reject = |field: &str, detail: String| -> GuardExit {
        tracing::error!(arm = %cfg.arm, field, detail, "guard: invalid invocation");
        GuardExit::ExecFailed
    };

    // R10-1：纯 argv 级，先于任何文件 I/O。
    if has_opt(&cfg.s0_args, FORBIDDEN_S0_OPT) {
        return Err(reject(
            FORBIDDEN_S0_OPT,
            "guard appends it; s0 args must not carry it (R10-1 single source)".to_string(),
        ));
    }
    if !cfg.s0_bin.is_absolute() {
        return Err(reject(
            "--s0-bin",
            format!("{} is not absolute", cfg.s0_bin.display()),
        ));
    }
    if !cfg.s0_bin.exists() {
        return Err(reject(
            "--s0-bin",
            format!("{} does not exist", cfg.s0_bin.display()),
        ));
    }

    // 既有 robot ID 选择本侧 arm-only/full，不另加布局开关。
    let hardware_id = identity::expected_hardware_id(cfg.arm, &cfg.robot_id).ok_or_else(|| {
        reject(
            "--robot-id",
            format!("不是 {} 的受支持运行身份: {:?}", cfg.arm, cfg.robot_id),
        )
    })?;

    // 转发给 robot-s0 的身份必须存在且一致——否则 daemon 会以别的身份
    // （或 ROBOT_ID env / "default"）跑起来，topic / latch / 计数全错位。
    for (opt, expected_value) in [
        (ROBOT_ID_OPT, cfg.robot_id.as_str()),
        (HARDWARE_ID_OPT, hardware_id),
    ] {
        let found = scan_opt(&cfg.s0_args, opt).map_err(|e| reject(opt, e))?;
        let Some(value) = found else {
            return Err(reject(opt, "missing in s0 args".to_string()));
        };
        if value != OsStr::new(expected_value) {
            return Err(reject(
                opt,
                format!("expected {expected_value:?}, got {}", value.display()),
            ));
        }
    }

    // latch 单源必须可解析（重复 / 缺值 / 等号写法一律拒绝）。
    let latch_path = cfg
        .resolve_latch_path()
        .map_err(|e| reject(LATCH_PATH_OPT, e))?;
    // S48 A3d / W1：共享的纯名称校验放在最早的 argv 级校验里——先于实例锁、
    // 连败计数与任何文件 I/O。非法名字是配置错误退 1（不是 86），因此即使锁
    // 被占用或预算已耗尽，也不会先产生锁 / 计数副作用再退别的码。
    if let Err(error) = validate_latch_path(&latch_path) {
        tracing::error!(
            arm = %cfg.arm,
            latch = %latch_path.display(),
            %error,
            "guard: latch name rejected before any lock or counter side effect"
        );
        return Err(GuardExit::LatchConfig);
    }
    if cfg.robot_id != identity::expected(cfg.arm).robot_id {
        let trust = scan_opt(&cfg.s0_args, RX_TRUST_DIR_OPT)
            .map_err(|e| reject(RX_TRUST_DIR_OPT, e))?
            .ok_or_else(|| reject(RX_TRUST_DIR_OPT, "完整总线必须显式绑定保护目录".to_string()))?;
        let trust = PathBuf::from(trust);
        if !latch_path.is_absolute()
            || !trust.is_absolute()
            || latch_path.parent() != Some(trust.as_path())
        {
            return Err(reject(
                RX_TRUST_DIR_OPT,
                "完整总线的绝对 latch 路径必须位于显式 RX 保护目录中".to_string(),
            ));
        }
    }
    Ok(())
}

#[cfg(any(feature = "xlerobot-real", test))]
fn verify_config_identity(cfg: &GuardConfig) -> Result<(), String> {
    let verified = identity::load_and_verify_arm_config(cfg.arm, &cfg.config_path)
        .map_err(|e| e.to_string())?;
    if verified.robot_id != cfg.robot_id {
        return Err(format!(
            "config [robot].id {:?} != guard robot_id {:?}",
            verified.robot_id, cfg.robot_id
        ));
    }
    tracing::info!(
        arm = %cfg.arm,
        robot_id = %verified.robot_id,
        hardware_id = %verified.hardware_id,
        address = %verified.address,
        "guard: bus identity verified"
    );
    Ok(())
}

/// 六步主流程。`Ok(())` 只在测试实现下可达（真实现 exec 成功不返回）。
///
/// # Errors
///
/// 见 [`GuardExit`]——每个变体都**绝不** exec `robot-s0`。
pub fn evaluate_guard<S: GuardSteps + ?Sized>(
    cfg: &GuardConfig,
    steps: &mut S,
) -> Result<(), GuardExit> {
    validate_invocation(cfg)?;
    // config 文件级身份校验同样在取锁之前：配置错误每次重试必然重现，
    // 不该消耗连败预算，且 86 本身已停掉自动重启。
    if let Err(reason) = steps.verify_identity(cfg) {
        tracing::error!(arm = %cfg.arm, config = %cfg.config_path.display(), %reason,
            "guard: D2 identity verification failed");
        return Err(GuardExit::ExecFailed);
    }
    let args = build_s0_args(cfg)?;

    // ① 持稳定 .lock。
    let lock = acquire_instance_lock(&cfg.lock_file()).map_err(|e| {
        tracing::error!(?e, robot_id = %cfg.robot_id, "guard: instance lock failed");
        match e {
            LockError::Conflict { .. } => GuardExit::LockConflict,
            LockError::Open { .. } => GuardExit::CounterIo,
        }
    })?;

    let count_file = cfg.count_file();
    // ① 读 n；n == 5 → 立即退 86（**不执行任何 pre 步**）。
    let n = read_count(&count_file).map_err(|e| {
        tracing::error!(?e, "guard: read count failed");
        GuardExit::CounterIo
    })?;
    if n >= MAX_CONSECUTIVE_FAILURES {
        tracing::error!(
            count = n,
            limit = MAX_CONSECUTIVE_FAILURES,
            "guard: consecutive failure budget exhausted; manual recovery required"
        );
        return Err(GuardExit::BudgetExhausted);
    }

    // ② 任何 pre 步之前先递增（未成功尝试数）。
    let attempt = n + 1;
    write_count_atomic(&count_file, attempt).map_err(|e| {
        tracing::error!(?e, attempt, "guard: pre-increment failed");
        GuardExit::CounterIo
    })?;

    // 两个 pre 步（顺序固定：先 bus 安全，再 latch provision）。
    if let Err(reason) = steps.preflight(cfg) {
        tracing::error!(arm = %cfg.arm, attempt, %reason, "guard: preflight failed");
        return Err(pre_step_exit(attempt, PREFLIGHT_FAIL_EXIT_CODE));
    }
    match steps.provision(cfg) {
        Ok(()) => {}
        Err(ProvisionStepError::LatchHeld { pid }) => {
            tracing::error!(arm = %cfg.arm, attempt, ?pid, "guard: latch lock held by another process");
            return Err(GuardExit::LatchHeld);
        }
        Err(ProvisionStepError::LatchConfig(reason)) => {
            tracing::error!(arm = %cfg.arm, attempt, %reason, "guard: latch lock/name/object rejected");
            return Err(GuardExit::LatchConfig);
        }
        Err(ProvisionStepError::Failed(reason)) => {
            tracing::error!(arm = %cfg.arm, attempt, %reason, "guard: latch provision failed");
            return Err(pre_step_exit(attempt, PROVISION_FAIL_EXIT_CODE));
        }
    }

    // ③ 两步全成功 → 清零 → exec。
    write_count_atomic(&count_file, 0).map_err(|e| {
        tracing::error!(?e, "guard: clearing count failed");
        GuardExit::CounterIo
    })?;
    // exec 前显式关闭锁 fd（fd 另置 FD_CLOEXEC 作兜底）。
    lock.close();

    match steps.exec_s0(cfg, &args) {
        ExecOutcome::Execed => Ok(()),
        ExecOutcome::Failed(reason) => {
            tracing::error!(s0_bin = %cfg.s0_bin.display(), %reason, "guard: exec failed");
            Err(GuardExit::ExecFailed)
        }
    }
}

/// pre 步失败的退出码：**当前值**（已递增）达上限 → 停止态，否则普通码。
const fn pre_step_exit(attempt: u8, plain: u8) -> GuardExit {
    if attempt >= MAX_CONSECUTIVE_FAILURES {
        GuardExit::BudgetExhausted
    } else {
        GuardExit::PreStepFailed(plain)
    }
}

/// 真实现：feetech 串口 preflight + latch provision + `exec`。
#[cfg(feature = "xlerobot-real")]
pub mod real {
    use std::ffi::OsString;
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    use super::super::preflight::{ensure_safe_arm, feetech::open_arm_bus};
    use super::super::provision::provision_latch;
    use super::{GuardConfig, GuardSteps, ProvisionStepError};

    /// 生产 [`GuardSteps`]。
    pub struct RealGuardSteps;

    impl GuardSteps for RealGuardSteps {
        fn verify_identity(&mut self, cfg: &GuardConfig) -> Result<(), String> {
            super::verify_config_identity(cfg)
        }

        fn preflight(&mut self, cfg: &GuardConfig) -> Result<(), String> {
            let mut bus = open_arm_bus(cfg.arm, &cfg.config_path)?;
            ensure_safe_arm(cfg.arm, &mut bus).map_err(|report| report.to_string())
        }

        fn provision(&mut self, cfg: &GuardConfig) -> Result<(), ProvisionStepError> {
            // validate_invocation 已保证可解析；这里再取一次保持单一来源。
            let path = cfg
                .resolve_latch_path()
                .map_err(ProvisionStepError::LatchConfig)?;
            // 稳定锁在 provision_latch 内获取并于返回时释放；exec 前不再持有。
            let outcome = provision_latch(&path).map_err(ProvisionStepError::from)?;
            tracing::info!(latch = %path.display(), ?outcome, "guard: latch provisioned");
            Ok(())
        }

        fn exec_s0(&mut self, cfg: &GuardConfig, args: &[OsString]) -> super::ExecOutcome {
            tracing::info!(
                s0_bin = %cfg.s0_bin.display(),
                arm = %cfg.arm,
                "guard: pre steps ok, exec robot-s0"
            );
            // exec 成功不返回；返回即失败。
            let err = Command::new(&cfg.s0_bin).args(args).exec();
            super::ExecOutcome::Failed(err.to_string())
        }
    }

    /// unit `ExecStart` 入口：跑六步，失败按 [`super::GuardExit::code`] 退出。
    pub fn run_guard(cfg: &GuardConfig) -> ! {
        match super::evaluate_guard(cfg, &mut RealGuardSteps) {
            // 真实现 exec 成功不返回；走到这里说明 exec 语义被破坏 → fail-closed。
            Ok(()) => {
                tracing::error!("guard: exec returned success unexpectedly");
                std::process::exit(i32::from(super::GUARD_STOP_EXIT_CODE))
            }
            Err(exit) => std::process::exit(i32::from(exit.code())),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use tempfile::TempDir;

    use super::*;

    /// 可编排的 [`GuardSteps`]：记录调用序 + 在 preflight 时窥探计数文件
    /// （证明 pre-increment 已生效）。
    struct ScriptedSteps {
        verify_config: bool,
        identity_result: Result<(), String>,
        preflight_result: Result<(), String>,
        provision_result: Result<(), ProvisionStepError>,
        exec: ExecOutcome,
        calls: RefCell<Vec<&'static str>>,
        seen_count: RefCell<Option<u8>>,
        seen_args: RefCell<Vec<OsString>>,
    }

    impl ScriptedSteps {
        fn ok() -> Self {
            Self {
                verify_config: false,
                identity_result: Ok(()),
                preflight_result: Ok(()),
                provision_result: Ok(()),
                exec: ExecOutcome::Execed,
                calls: RefCell::new(Vec::new()),
                seen_count: RefCell::new(None),
                seen_args: RefCell::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<&'static str> {
            self.calls.borrow().clone()
        }
    }

    impl GuardSteps for ScriptedSteps {
        fn verify_identity(&mut self, cfg: &GuardConfig) -> Result<(), String> {
            self.calls.borrow_mut().push("verify_identity");
            if self.verify_config {
                verify_config_identity(cfg)
            } else {
                self.identity_result.clone()
            }
        }

        fn preflight(&mut self, cfg: &GuardConfig) -> Result<(), String> {
            self.calls.borrow_mut().push("preflight");
            *self.seen_count.borrow_mut() = read_count(&cfg.count_file()).ok();
            self.preflight_result.clone()
        }

        fn provision(&mut self, cfg: &GuardConfig) -> Result<(), ProvisionStepError> {
            let _ = cfg;
            self.calls.borrow_mut().push("provision");
            self.provision_result.clone()
        }

        fn exec_s0(&mut self, cfg: &GuardConfig, args: &[OsString]) -> ExecOutcome {
            let _ = cfg;
            self.calls.borrow_mut().push("exec");
            *self.seen_args.borrow_mut() = args.to_vec();
            self.exec.clone()
        }
    }

    struct Fixture {
        dir: TempDir,
        cfg: GuardConfig,
    }

    fn fixture() -> Fixture {
        let dir = TempDir::new().unwrap();
        let s0_bin = dir.path().join("robot-s0");
        std::fs::write(&s0_bin, b"#!/bin/true\n").unwrap();
        let runtime_dir = dir.path().join("run");
        std::fs::create_dir(&runtime_dir).unwrap();
        let cfg = GuardConfig {
            arm: DeployArm::Left,
            config_path: dir.path().join("xlerobot-left-arm.toml"),
            s0_bin,
            runtime_dir,
            robot_id: "xlerobot_left".to_string(),
            s0_args: vec![
                OsString::from("--robot-id"),
                OsString::from("xlerobot_left"),
                OsString::from("--hardware-id"),
                OsString::from("left_arm"),
                OsString::from("--latch-path"),
                dir.path().join("estop_latch").into_os_string(),
            ],
        };
        Fixture { dir, cfg }
    }

    /// D2 合法的最小身份 argv（不含 `--latch-path`）。
    fn identity_args() -> Vec<OsString> {
        vec![
            OsString::from("--robot-id"),
            OsString::from("xlerobot_left"),
            OsString::from("--hardware-id"),
            OsString::from("left_arm"),
        ]
    }

    fn full_fixture(arm: DeployArm) -> Fixture {
        let mut f = fixture();
        let (robot_id, hardware_id, config) = match arm {
            DeployArm::Left => (
                "xlerobot_bus1_full",
                "left_arm_full_with_head",
                "xlerobot-bus1.toml",
            ),
            DeployArm::Right => (
                "xlerobot_bus2_full",
                "right_arm_full_with_base",
                "xlerobot-bus2.toml",
            ),
        };
        let trust = f.dir.path().join("protection");
        f.cfg.arm = arm;
        f.cfg.robot_id = robot_id.to_string();
        f.cfg.config_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../configs")
            .join(config);
        f.cfg.s0_args = vec![
            OsString::from(ROBOT_ID_OPT),
            OsString::from(robot_id),
            OsString::from(HARDWARE_ID_OPT),
            OsString::from(hardware_id),
            OsString::from(LATCH_PATH_OPT),
            trust.join("estop_latch").into_os_string(),
            OsString::from(RX_TRUST_DIR_OPT),
            trust.into_os_string(),
        ];
        f
    }

    fn count_of(cfg: &GuardConfig) -> u8 {
        read_count(&cfg.count_file()).expect("count")
    }

    /// 成功路径：pre 步顺序固定 → 清零 → exec；argv 自动追加 `--config`。
    #[test]
    fn success_clears_count_and_execs_with_appended_config() {
        let f = fixture();
        let mut steps = ScriptedSteps::ok();
        evaluate_guard(&f.cfg, &mut steps).expect("成功路径");
        assert_eq!(
            steps.calls(),
            vec!["verify_identity", "preflight", "provision", "exec"]
        );
        assert_eq!(count_of(&f.cfg), 0, "成功必须清零");
        let args = steps.seen_args.borrow().clone();
        assert_eq!(args[args.len() - 2], OsString::from("--config"));
        assert_eq!(
            args[args.len() - 1],
            f.cfg.config_path.clone().into_os_string()
        );
        // exec 前锁已显式关闭 → 可以再次获取。
        acquire_instance_lock(&f.cfg.lock_file()).expect("锁必须已释放");
    }

    #[test]
    fn full_configs_pass_identity_before_the_original_guard_steps() {
        for arm in [DeployArm::Left, DeployArm::Right] {
            let f = full_fixture(arm);
            let mut steps = ScriptedSteps::ok();
            steps.verify_config = true;
            evaluate_guard(&f.cfg, &mut steps).expect("完整总线应沿原六步通过");
            assert_eq!(
                steps.calls(),
                ["verify_identity", "preflight", "provision", "exec"]
            );
            assert_eq!(*steps.seen_count.borrow(), Some(1));
            assert_eq!(count_of(&f.cfg), 0);
        }
    }

    #[test]
    fn full_guard_rejects_missing_or_ambiguous_protection_before_any_step() {
        for arm in [DeployArm::Left, DeployArm::Right] {
            for opt in [LATCH_PATH_OPT, RX_TRUST_DIR_OPT] {
                let mut f = full_fixture(arm);
                let index = f.cfg.s0_args.iter().position(|arg| arg == opt).unwrap();
                f.cfg.s0_args.drain(index..index + 2);
                let mut steps = ScriptedSteps::ok();
                assert_eq!(
                    evaluate_guard(&f.cfg, &mut steps),
                    Err(GuardExit::ExecFailed)
                );
                assert_eq!(steps.calls(), Vec::<&str>::new());
                assert!(!f.cfg.lock_file().exists());
                assert!(!f.cfg.count_file().exists());
            }
            for trust in [
                OsString::from("relative"),
                OsString::from("/another/protection"),
            ] {
                let mut f = full_fixture(arm);
                *f.cfg.s0_args.last_mut().unwrap() = trust;
                let mut steps = ScriptedSteps::ok();
                assert_eq!(
                    evaluate_guard(&f.cfg, &mut steps),
                    Err(GuardExit::ExecFailed)
                );
                assert_eq!(steps.calls(), Vec::<&str>::new());
                assert!(!f.cfg.lock_file().exists());
                assert!(!f.cfg.count_file().exists());
            }
            let mut f = full_fixture(arm);
            f.cfg.s0_args.extend([
                OsString::from(RX_TRUST_DIR_OPT),
                OsString::from("/duplicate"),
            ]);
            let mut steps = ScriptedSteps::ok();
            assert_eq!(
                evaluate_guard(&f.cfg, &mut steps),
                Err(GuardExit::ExecFailed)
            );
            assert_eq!(steps.calls(), Vec::<&str>::new());
            assert!(!f.cfg.lock_file().exists());
            assert!(!f.cfg.count_file().exists());
        }
    }

    #[test]
    fn full_and_arm_only_identity_cannot_be_mixed_before_lock_or_counter() {
        for arm in [DeployArm::Left, DeployArm::Right] {
            let mut f = full_fixture(arm);
            let index = f
                .cfg
                .s0_args
                .iter()
                .position(|arg| arg == HARDWARE_ID_OPT)
                .unwrap();
            f.cfg.s0_args[index + 1] = OsString::from(identity::expected(arm).hardware_id);
            let mut steps = ScriptedSteps::ok();
            assert_eq!(
                evaluate_guard(&f.cfg, &mut steps),
                Err(GuardExit::ExecFailed)
            );
            assert_eq!(steps.calls(), Vec::<&str>::new());
            assert!(!f.cfg.lock_file().exists());
            assert!(!f.cfg.count_file().exists());

            let mut f = full_fixture(arm);
            f.cfg.config_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../configs")
                .join(format!("xlerobot-{}-arm.toml", arm.label()));
            let mut steps = ScriptedSteps::ok();
            steps.verify_config = true;
            assert_eq!(
                evaluate_guard(&f.cfg, &mut steps),
                Err(GuardExit::ExecFailed)
            );
            assert_eq!(steps.calls(), ["verify_identity"]);
            assert!(!f.cfg.lock_file().exists());
            assert!(!f.cfg.count_file().exists());

            let mut f = full_fixture(arm);
            let other = match arm {
                DeployArm::Left => DeployArm::Right,
                DeployArm::Right => DeployArm::Left,
            };
            f.cfg.arm = other;
            let mut steps = ScriptedSteps::ok();
            assert_eq!(
                evaluate_guard(&f.cfg, &mut steps),
                Err(GuardExit::ExecFailed)
            );
            assert_eq!(steps.calls(), Vec::<&str>::new());
            assert!(!f.cfg.lock_file().exists());
            assert!(!f.cfg.count_file().exists());
        }
    }

    /// **pre-increment 硬契约**：preflight 执行时计数文件已经是 n+1。
    #[test]
    fn count_is_incremented_before_any_pre_step() {
        let f = fixture();
        let mut steps = ScriptedSteps::ok();
        evaluate_guard(&f.cfg, &mut steps).expect("成功路径");
        assert_eq!(
            *steps.seen_count.borrow(),
            Some(1),
            "preflight 之前必须已写入 n+1"
        );
    }

    /// count == 5 → 立即退 86，**不执行任何 pre 步**，计数不变。
    #[test]
    fn budget_exhausted_exits_86_without_pre_steps() {
        let f = fixture();
        write_count_atomic(&f.cfg.count_file(), MAX_CONSECUTIVE_FAILURES).unwrap();
        let mut steps = ScriptedSteps::ok();
        let exit = evaluate_guard(&f.cfg, &mut steps).expect_err("必须停止态");
        assert_eq!(exit, GuardExit::BudgetExhausted);
        assert_eq!(exit.code(), 86);
        assert_eq!(
            steps.calls(),
            vec!["verify_identity"],
            "身份校验之外不得执行任何 pre 步"
        );
        assert_eq!(count_of(&f.cfg), MAX_CONSECUTIVE_FAILURES);
    }

    /// 连续 5 次未成功 → 前 4 次普通码，第 5 次退 86；第 6 次直接停止态。
    #[test]
    fn five_consecutive_failures_escalate_to_stop_state() {
        let f = fixture();
        for attempt in 1..=5u8 {
            let mut steps = ScriptedSteps::ok();
            steps.preflight_result = Err("bus down".into());
            let exit = evaluate_guard(&f.cfg, &mut steps).expect_err("必须失败");
            if attempt < 5 {
                assert_eq!(
                    exit,
                    GuardExit::PreStepFailed(PREFLIGHT_FAIL_EXIT_CODE),
                    "第 {attempt} 次应走普通码重试"
                );
                assert_eq!(exit.code(), 70);
            } else {
                assert_eq!(exit, GuardExit::BudgetExhausted, "第 5 次进停止态");
                assert_eq!(exit.code(), 86);
            }
            assert_eq!(count_of(&f.cfg), attempt);
            assert_eq!(
                steps.calls(),
                vec!["verify_identity", "preflight"],
                "provision/exec 不得执行"
            );
        }
        // 第 6 次：读到 5 → 直接停止态。
        let mut steps = ScriptedSteps::ok();
        assert_eq!(
            evaluate_guard(&f.cfg, &mut steps).expect_err("停止态"),
            GuardExit::BudgetExhausted
        );
        assert_eq!(steps.calls(), vec!["verify_identity"]);
    }

    /// 一次成功清零预算，之后重新从 1 累计。
    #[test]
    fn success_resets_the_budget() {
        let f = fixture();
        for _ in 0..3 {
            let mut steps = ScriptedSteps::ok();
            steps.provision_result = Err(ProvisionStepError::Failed("latch broken".into()));
            let exit = evaluate_guard(&f.cfg, &mut steps).expect_err("必须失败");
            assert_eq!(exit, GuardExit::PreStepFailed(PROVISION_FAIL_EXIT_CODE));
            assert_eq!(exit.code(), 71);
        }
        assert_eq!(count_of(&f.cfg), 3);
        evaluate_guard(&f.cfg, &mut ScriptedSteps::ok()).expect("成功");
        assert_eq!(count_of(&f.cfg), 0);
        let mut steps = ScriptedSteps::ok();
        steps.preflight_result = Err("bus down".into());
        evaluate_guard(&f.cfg, &mut steps).expect_err("再失败");
        assert_eq!(count_of(&f.cfg), 1, "清零后重新累计");
    }

    /// 计数非法格式 / 越界 → `CounterIo` 退 86，不执行 pre 步。
    #[test]
    fn malformed_or_out_of_range_count_exits_86() {
        for raw in ["garbage", "-1", "9", ""] {
            let f = fixture();
            std::fs::write(f.cfg.count_file(), raw).unwrap();
            let mut steps = ScriptedSteps::ok();
            let exit = evaluate_guard(&f.cfg, &mut steps).expect_err("必须 fail-closed");
            assert_eq!(exit, GuardExit::CounterIo, "raw={raw:?}");
            assert_eq!(exit.code(), 86);
            assert_eq!(steps.calls(), vec!["verify_identity"], "raw={raw:?}");
        }
    }

    /// 同实例并发（锁被占）→ `LockConflict` 退 86。
    #[test]
    fn lock_conflict_exits_86() {
        let f = fixture();
        let held = acquire_instance_lock(&f.cfg.lock_file()).expect("先占锁");
        let mut steps = ScriptedSteps::ok();
        let exit = evaluate_guard(&f.cfg, &mut steps).expect_err("必须冲突");
        assert_eq!(exit, GuardExit::LockConflict);
        assert_eq!(exit.code(), 86);
        assert_eq!(steps.calls(), vec!["verify_identity"]);
        held.close();
    }

    /// `s0_bin` 非绝对路径 / 不存在 → 退 86，且**不碰计数**（不消耗预算）。
    #[test]
    fn invalid_s0_bin_exits_86_without_touching_count() {
        let f = fixture();
        for bad in [PathBuf::from("robot-s0"), f.cfg.runtime_dir.join("nope")] {
            let mut cfg = f.cfg.clone();
            cfg.s0_bin = bad.clone();
            let mut steps = ScriptedSteps::ok();
            let exit = evaluate_guard(&cfg, &mut steps).expect_err("必须拒绝");
            assert_eq!(exit, GuardExit::ExecFailed, "{}", bad.display());
            assert_eq!(exit.code(), 86);
            assert!(
                steps.calls().is_empty(),
                "不得进入任何步骤，实际 {:?}",
                steps.calls()
            );
            assert!(!cfg.count_file().exists(), "不得消耗连败预算");
        }
    }

    /// R10-1：`--` 后自带 `--config`（两种写法）→ 配置错误退 86。
    #[test]
    fn duplicate_config_in_s0_args_exits_86() {
        let f = fixture();
        for extra in [
            vec![
                OsString::from("--config"),
                OsString::from("/etc/other.toml"),
            ],
            vec![OsString::from("--config=/etc/other.toml")],
        ] {
            let mut cfg = f.cfg.clone();
            cfg.s0_args.extend(extra.clone());
            let mut steps = ScriptedSteps::ok();
            let exit = evaluate_guard(&cfg, &mut steps).expect_err("必须拒绝");
            assert_eq!(exit, GuardExit::ExecFailed, "{extra:?}");
            assert!(
                steps.calls().is_empty(),
                "不得进入任何步骤，实际 {:?}",
                steps.calls()
            );
            assert!(!cfg.count_file().exists());
        }
    }

    /// latch 路径单源：唯一的空格形式 `--latch-path <p>` 跟随，缺省按
    /// `robot_id` 回填（与 `robot-s0` 默认规则逐字一致）。
    #[test]
    fn latch_path_follows_s0_args_then_robot_id() {
        let f = fixture();
        let expected = f.cfg.runtime_dir.parent().unwrap().join("estop_latch");
        assert_eq!(f.cfg.resolve_latch_path().expect("单源可解析"), expected);

        let mut cfg = f.cfg.clone();
        cfg.s0_args = identity_args();
        assert_eq!(
            cfg.resolve_latch_path().expect("缺省回填"),
            PathBuf::from("/var/lib/robot/xlerobot_left/estop_latch")
        );
    }

    /// **round 1 P0-3**：`robot-s0` 对重复取最后一个、不认等号写法；guard
    /// 取第一个且曾接受等号写法 → 两侧可能 provision A、读 B。修法 = 三种
    /// 歧义写法**一律拒绝**，且必须在**取锁之前**（不消耗连败预算）。
    #[test]
    fn ambiguous_latch_path_forms_are_rejected_before_the_lock() {
        let f = fixture();
        let cases: [(&str, Vec<OsString>); 3] = [
            (
                "等号写法（daemon 不认识）",
                vec![OsString::from("--latch-path=/var/lib/robot/x/estop_latch")],
            ),
            (
                "重复（daemon 取最后 / guard 取第一）",
                vec![
                    OsString::from("--latch-path"),
                    OsString::from("/var/lib/robot/a/estop_latch"),
                    OsString::from("--latch-path"),
                    OsString::from("/var/lib/robot/b/estop_latch"),
                ],
            ),
            ("缺取值", vec![OsString::from("--latch-path")]),
        ];
        for (label, extra) in cases {
            let mut cfg = f.cfg.clone();
            cfg.s0_args = identity_args();
            cfg.s0_args.extend(extra);
            assert!(cfg.resolve_latch_path().is_err(), "{label}");
            let mut steps = ScriptedSteps::ok();
            let exit = evaluate_guard(&cfg, &mut steps).expect_err(label);
            assert_eq!(exit, GuardExit::ExecFailed, "{label}");
            assert_eq!(exit.code(), 86, "{label}");
            assert!(steps.calls().is_empty(), "{label}: 不得进入任何步骤");
            assert!(!cfg.count_file().exists(), "{label}: 不得消耗连败预算");
        }
    }

    /// **round 1 P0-1（argv 侧）**：转发给 `robot-s0` 的身份必须存在且
    /// 与 D2 一致——缺失会让 daemon 落到 `ROBOT_ID` env / `"default"`。
    #[test]
    fn forwarded_identity_must_match_d2() {
        let f = fixture();
        let latch: Vec<OsString> = vec![
            OsString::from("--latch-path"),
            OsString::from("/var/lib/robot/xlerobot_left/estop_latch"),
        ];
        let cases: [(&str, Vec<OsString>); 5] = [
            (
                "缺 --robot-id",
                vec![OsString::from("--hardware-id"), OsString::from("left_arm")],
            ),
            (
                "缺 --hardware-id",
                vec![
                    OsString::from("--robot-id"),
                    OsString::from("xlerobot_left"),
                ],
            ),
            (
                "--robot-id 是另一条臂",
                vec![
                    OsString::from("--robot-id"),
                    OsString::from("xlerobot_right"),
                    OsString::from("--hardware-id"),
                    OsString::from("left_arm"),
                ],
            ),
            (
                "--hardware-id 是另一条臂",
                vec![
                    OsString::from("--robot-id"),
                    OsString::from("xlerobot_left"),
                    OsString::from("--hardware-id"),
                    OsString::from("right_arm"),
                ],
            ),
            (
                "--robot-id 重复",
                vec![
                    OsString::from("--robot-id"),
                    OsString::from("xlerobot_left"),
                    OsString::from("--robot-id"),
                    OsString::from("xlerobot_right"),
                    OsString::from("--hardware-id"),
                    OsString::from("left_arm"),
                ],
            ),
        ];
        for (label, mut args) in cases {
            args.extend(latch.clone());
            let mut cfg = f.cfg.clone();
            cfg.s0_args = args;
            let mut steps = ScriptedSteps::ok();
            let exit = evaluate_guard(&cfg, &mut steps).expect_err(label);
            assert_eq!(exit, GuardExit::ExecFailed, "{label}");
            assert!(steps.calls().is_empty(), "{label}");
            assert!(!cfg.count_file().exists(), "{label}: 不得消耗连败预算");
        }
    }

    /// guard 自身的 `robot_id` 必须就是本臂 D2 身份。
    #[test]
    fn guard_robot_id_must_match_the_arm() {
        let f = fixture();
        let mut cfg = f.cfg.clone();
        cfg.robot_id = "xlerobot_right".to_string();
        let mut steps = ScriptedSteps::ok();
        assert_eq!(
            evaluate_guard(&cfg, &mut steps).expect_err("必须拒绝"),
            GuardExit::ExecFailed
        );
        assert!(
            steps.calls().is_empty(),
            "不得进入任何步骤，实际 {:?}",
            steps.calls()
        );
    }

    /// **round 1 P0-1（config 侧）**：文件级身份校验失败 → 退 86，且在取锁
    /// 之前（不消耗连败预算、绝不进 preflight）。
    #[test]
    fn config_identity_failure_exits_86_without_touching_count() {
        let f = fixture();
        let mut steps = ScriptedSteps::ok();
        steps.identity_result = Err("[robot].id mismatch".into());
        let exit = evaluate_guard(&f.cfg, &mut steps).expect_err("必须拒绝");
        assert_eq!(exit, GuardExit::ExecFailed);
        assert_eq!(exit.code(), 86);
        assert_eq!(steps.calls(), vec!["verify_identity"], "不得进入 preflight");
        assert!(!f.cfg.count_file().exists(), "不得消耗连败预算");
    }

    /// exec 返回 = 失败 → 退 86（计数此时已清零，属"已成功的 guard 尝试"）。
    #[test]
    fn exec_failure_exits_86_after_count_cleared() {
        let f = fixture();
        let mut steps = ScriptedSteps::ok();
        steps.exec = ExecOutcome::Failed("ENOENT".into());
        let exit = evaluate_guard(&f.cfg, &mut steps).expect_err("exec 失败");
        assert_eq!(exit, GuardExit::ExecFailed);
        assert_eq!(exit.code(), 86);
        assert_eq!(count_of(&f.cfg), 0);
    }

    /// 清零失败（count 路径被目录占位，rename 必失败）→ `CounterIo` 退 86，
    /// **绝不 exec**。
    #[test]
    fn clear_failure_exits_86_without_exec() {
        let f = fixture();
        let mut steps = ScriptedSteps::ok();
        // 预置一次递增，随后把 count 路径换成目录 → 后续 rename 失败。
        write_count_atomic(&f.cfg.count_file(), 1).unwrap();
        std::fs::remove_file(f.cfg.count_file()).unwrap();
        std::fs::create_dir(f.cfg.count_file()).unwrap();
        let exit = evaluate_guard(&f.cfg, &mut steps).expect_err("必须 fail-closed");
        assert_eq!(exit, GuardExit::CounterIo);
        assert!(!steps.calls().contains(&"exec"), "{:?}", steps.calls());
    }

    /// AC-E3 回归锁：**86 是 guard 永久保留码**——`robot-s0` 任何路径都
    /// 不得以它退出，否则 unit 的 `RestartPreventExitStatus=86` 会连
    /// watchdog 重启一起吞掉（违反 §6.7 #2）。源级扫描 bin 里的
    /// `exit(<字面量>)` / `Err(<字面量>)`；post-E 只扩允许集，86 永久保留。
    #[test]
    fn s0_daemon_never_exits_with_the_reserved_code() {
        const SRC: &str = include_str!("../bin/robot-s0.rs");

        fn literal_codes(src: &str, marker: &str) -> Vec<u8> {
            src.match_indices(marker)
                .filter_map(|(i, _)| {
                    let rest = &src[i + marker.len()..];
                    let end = rest.find(')')?;
                    rest[..end].trim().parse::<u8>().ok()
                })
                .collect()
        }

        // 生产出口与测试子进程分开；测试用 101 报断言失败，不属于 daemon 退出码。
        let (production, _) = SRC
            .split_once("\n#[cfg(test)]\nmod tests {")
            .expect("daemon 测试模块边界必须明确，不能静默漏扫生产源码");
        let mut codes = literal_codes(production, "exit(");
        codes.extend(literal_codes(production, "Err("));
        assert!(!codes.is_empty(), "扫描失效（marker 漂移）");
        for code in codes {
            assert_ne!(code, GUARD_STOP_EXIT_CODE, "robot-s0 不得以 86 退出");
            assert!(
                matches!(code, 0..=4 | 87 | 88),
                "S0 出现未批准的退出码 {code}"
            );
        }
    }

    /// 退出码表：只有 `PreStepFailed` 是普通码，其余全 86。
    #[test]
    fn exit_code_table_is_locked() {
        assert_eq!(GuardExit::ExecFailed.code(), 86);
        assert_eq!(GuardExit::BudgetExhausted.code(), 86);
        assert_eq!(GuardExit::CounterIo.code(), 86);
        assert_eq!(GuardExit::LockConflict.code(), 86);
        assert_eq!(GuardExit::PreStepFailed(70).code(), 70);
        assert!(GuardExit::LockConflict.is_stop_state());
        assert!(!GuardExit::PreStepFailed(70).is_stop_state());
        // 86 不得与 robot-s0(0/1/2) / S1(3/4/5) 既有码重叠。
        assert!(!(0..=5).contains(&GUARD_STOP_EXIT_CODE));
        assert_ne!(PREFLIGHT_FAIL_EXIT_CODE, GUARD_STOP_EXIT_CODE);
        assert_ne!(PROVISION_FAIL_EXIT_CODE, GUARD_STOP_EXIT_CODE);
    }

    // ── S48 A3d / W1：latch 稳定锁的类型化分流 ─────────────────────────────

    /// 锁被占用 → `LatchHeld` 退 89：不落入 71 / 86，计数保留已递增值（未成功尝试），
    /// 不清零、不 exec。
    #[test]
    fn latch_held_exits_89_and_keeps_incremented_count() {
        let f = fixture();
        let mut steps = ScriptedSteps::ok();
        steps.provision_result = Err(ProvisionStepError::LatchHeld { pid: Some(4242) });
        let exit = evaluate_guard(&f.cfg, &mut steps).expect_err("必须失败");
        assert_eq!(exit, GuardExit::LatchHeld);
        assert_eq!(exit.code(), 89);
        assert!(!exit.is_stop_state());
        assert_eq!(count_of(&f.cfg), 1, "未成功尝试计数保留");
        assert_eq!(
            steps.calls(),
            vec!["verify_identity", "preflight", "provision"],
            "不得 exec"
        );
        // 第 5 次仍是 89，不被预算映射改写成 86。
        write_count_atomic(&f.cfg.count_file(), MAX_CONSECUTIVE_FAILURES - 1).unwrap();
        let mut steps = ScriptedSteps::ok();
        steps.provision_result = Err(ProvisionStepError::LatchHeld { pid: None });
        assert_eq!(
            evaluate_guard(&f.cfg, &mut steps).expect_err("必须失败"),
            GuardExit::LatchHeld
        );
        assert_eq!(count_of(&f.cfg), MAX_CONSECUTIVE_FAILURES);
    }

    /// 名字 / 锁 IO / 对象身份错误 → `LatchConfig` 退 1：同样不落 71 / 86。
    #[test]
    fn latch_config_error_exits_1() {
        let f = fixture();
        let mut steps = ScriptedSteps::ok();
        steps.provision_result = Err(ProvisionStepError::LatchConfig("symlink".into()));
        let exit = evaluate_guard(&f.cfg, &mut steps).expect_err("必须失败");
        assert_eq!(exit, GuardExit::LatchConfig);
        assert_eq!(exit.code(), 1);
        assert_eq!(count_of(&f.cfg), 1);
        assert_eq!(
            steps.calls(),
            vec!["verify_identity", "preflight", "provision"]
        );
    }

    /// `ProvisionError` → `ProvisionStepError` 的三类映射，与退出码分区一致。
    #[test]
    fn provision_error_maps_to_typed_step_error() {
        use super::super::provision::ProvisionError;
        use crate::latch_lock::{LatchLockError, LatchNameError, LockStage};
        assert_eq!(
            ProvisionStepError::from(ProvisionError::LatchLockHeld { pid: Some(7) }),
            ProvisionStepError::LatchHeld { pid: Some(7) }
        );
        assert!(matches!(
            ProvisionStepError::from(ProvisionError::InvalidLatchName(LatchNameError::EmptyOrDot)),
            ProvisionStepError::LatchConfig(_)
        ));
        assert!(matches!(
            ProvisionStepError::from(ProvisionError::LatchLock(LatchLockError::Identity {
                stage: LockStage::InspectLock,
                detail: "x",
            })),
            ProvisionStepError::LatchConfig(_)
        ));
        assert!(matches!(
            ProvisionStepError::from(ProvisionError::Write {
                path: PathBuf::from("p"),
                source: std::io::Error::other("x"),
            }),
            ProvisionStepError::Failed(_)
        ));
    }

    /// 真实 provision 接在 guard 六步里：锁在 provision 返回时释放，exec 时可由
    /// "daemon"（这里用同进程再取锁模拟）重新获取；测试持锁时 guard 退 89。
    struct RealProvisionSteps {
        calls: RefCell<Vec<&'static str>>,
        lock_free_at_exec: RefCell<Option<bool>>,
    }

    impl GuardSteps for RealProvisionSteps {
        fn verify_identity(&mut self, _cfg: &GuardConfig) -> Result<(), String> {
            self.calls.borrow_mut().push("verify_identity");
            Ok(())
        }

        fn preflight(&mut self, _cfg: &GuardConfig) -> Result<(), String> {
            self.calls.borrow_mut().push("preflight");
            Ok(())
        }

        fn provision(&mut self, cfg: &GuardConfig) -> Result<(), ProvisionStepError> {
            self.calls.borrow_mut().push("provision");
            let path = cfg
                .resolve_latch_path()
                .map_err(ProvisionStepError::LatchConfig)?;
            super::super::provision::provision_latch(&path)
                .map(|_| ())
                .map_err(ProvisionStepError::from)
        }

        fn exec_s0(&mut self, cfg: &GuardConfig, _args: &[OsString]) -> ExecOutcome {
            self.calls.borrow_mut().push("exec");
            let path = cfg.resolve_latch_path().unwrap();
            let free = crate::latch_lock::acquire_latch_lock(&path).is_ok();
            *self.lock_free_at_exec.borrow_mut() = Some(free);
            ExecOutcome::Execed
        }
    }

    #[test]
    fn real_provision_releases_latch_lock_before_exec_and_maps_held_to_89() {
        let f = fixture();
        let latch = f.cfg.resolve_latch_path().unwrap();
        let mut steps = RealProvisionSteps {
            calls: RefCell::new(Vec::new()),
            lock_free_at_exec: RefCell::new(None),
        };
        evaluate_guard(&f.cfg, &mut steps).expect("成功路径");
        assert_eq!(
            *steps.calls.borrow(),
            vec!["verify_identity", "preflight", "provision", "exec"]
        );
        assert_eq!(
            *steps.lock_free_at_exec.borrow(),
            Some(true),
            "exec 时锁必须已释放"
        );
        assert_eq!(std::fs::read_to_string(&latch).unwrap(), "1\n");
        assert_eq!(count_of(&f.cfg), 0);

        let holder = crate::latch_lock::acquire_latch_lock(&latch).unwrap();
        let mut steps = RealProvisionSteps {
            calls: RefCell::new(Vec::new()),
            lock_free_at_exec: RefCell::new(None),
        };
        let exit = evaluate_guard(&f.cfg, &mut steps).expect_err("持锁时必须 89");
        assert_eq!(exit, GuardExit::LatchHeld);
        assert_eq!(
            *steps.calls.borrow(),
            vec!["verify_identity", "preflight", "provision"]
        );
        assert_eq!(count_of(&f.cfg), 1);
        drop(holder);

        // 非法 latch 名字：validate_invocation 的纯名称校验就拒绝 → 1，不进任何步骤。
        let mut cfg = f.cfg.clone();
        let idx = cfg
            .s0_args
            .iter()
            .position(|a| a == "--latch-path")
            .unwrap();
        cfg.s0_args[idx + 1] = f
            .cfg
            .runtime_dir
            .parent()
            .unwrap()
            .join("estop_latch.bak")
            .into_os_string();
        let mut steps = RealProvisionSteps {
            calls: RefCell::new(Vec::new()),
            lock_free_at_exec: RefCell::new(None),
        };
        let exit = evaluate_guard(&cfg, &mut steps).expect_err("非法名字必须 1");
        assert_eq!(exit, GuardExit::LatchConfig);
        assert_eq!(exit.code(), 1);
        assert!(steps.calls.borrow().is_empty(), "名字在 argv 校验就被拒绝");
    }

    /// W1 预审回归：非法 latch 名字在**最早的 argv 校验**退 1，且在无锁 / 实例锁被占 /
    /// 预算耗尽三类条件下都不创建或修改锁与计数文件，也不进入任何步骤——不会先产生
    /// 副作用再退 86。
    #[test]
    fn invalid_latch_name_exits_1_without_lock_or_counter_side_effects() {
        for bad in [
            "estop_latch.bak",
            "estop_latch.lock",
            "rx_trust_marker",
            "x.tmp.1.2",
        ] {
            let f = fixture();
            let mut cfg = f.cfg.clone();
            let idx = cfg
                .s0_args
                .iter()
                .position(|a| a == "--latch-path")
                .unwrap();
            cfg.s0_args[idx + 1] = f
                .cfg
                .runtime_dir
                .parent()
                .unwrap()
                .join(bad)
                .into_os_string();

            // ① 无锁、无计数。
            let mut steps = ScriptedSteps::ok();
            let exit = evaluate_guard(&cfg, &mut steps).expect_err(bad);
            assert_eq!(exit, GuardExit::LatchConfig, "{bad}");
            assert_eq!(exit.code(), 1, "{bad}");
            assert!(steps.calls().is_empty(), "{bad}: 不得进入任何步骤");
            assert!(!cfg.count_file().exists(), "{bad}: 不得创建计数");
            assert!(!cfg.lock_file().exists(), "{bad}: 不得创建实例锁");

            // ② 实例锁被另一持有者占用：仍退 1，不是 86 LockConflict；锁文件字节不变。
            let held = acquire_instance_lock(&cfg.lock_file()).expect("先占锁");
            let lock_before = std::fs::read(cfg.lock_file()).unwrap();
            let mut steps = ScriptedSteps::ok();
            let exit = evaluate_guard(&cfg, &mut steps).expect_err(bad);
            assert_eq!(exit, GuardExit::LatchConfig, "{bad}");
            assert!(steps.calls().is_empty(), "{bad}");
            assert_eq!(
                std::fs::read(cfg.lock_file()).unwrap(),
                lock_before,
                "{bad}"
            );
            assert!(!cfg.count_file().exists(), "{bad}: 不得创建计数");
            held.close();

            // ③ 预算已耗尽：仍退 1，不是 86 BudgetExhausted；计数不变。
            write_count_atomic(&cfg.count_file(), MAX_CONSECUTIVE_FAILURES).unwrap();
            let count_before = std::fs::read(cfg.count_file()).unwrap();
            let mut steps = ScriptedSteps::ok();
            let exit = evaluate_guard(&cfg, &mut steps).expect_err(bad);
            assert_eq!(exit, GuardExit::LatchConfig, "{bad}");
            assert!(steps.calls().is_empty(), "{bad}");
            assert_eq!(
                std::fs::read(cfg.count_file()).unwrap(),
                count_before,
                "{bad}"
            );
            assert_eq!(count_of(&cfg), MAX_CONSECUTIVE_FAILURES, "{bad}");
        }
    }
}
