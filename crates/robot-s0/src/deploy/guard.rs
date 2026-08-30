//! **guard**：arm S0 unit 的 `ExecStart` 主进程（v0.5 候选 E 段(9)，
//! Spec D13 r8 Y1 + r11 AB1 六步 + P.2.9 QA1/R10-1 + AC-E22）。
//!
//! ## 为什么 guard 必须是主进程（QA1 / Y1 终拍）
//!
//! `RestartPreventExitStatus=86` **只作用于主进程**。若 pre 步放
//! `ExecStartPre=`，退 86 不会被捕获 → 连败停止态失效。故 unit 写
//! `ExecStart=xlerobot_deploy guard …`，guard 末尾 `exec s0_daemon`
//! 保住 `MainPID`（`Type=notify` / watchdog 语义照旧）。
//!
//! ## 六步（pre-increment，r11 AB1）
//!
//! 1. 持稳定 `.lock`，读 `n`；`n == 5` → 立即退 86；
//! 2. **执行任何 preflight / provision 之前**原子写入 `n+1`（写失败 → 86）；
//! 3. 两步全部成功 → **清零计数**（清零失败退 86）→ `exec s0_daemon`；
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
//! ## 身份绑定（round 1 P0-1 / P0-3）
//!
//! `--arm` 决定**安全写序**，config 决定**物理 bus**，s0 argv 决定 daemon
//! 的**运行身份**——三者必须锁在同一条 D2 行上，否则会静默操作错 bus。
//! 故 guard 在**取锁之前**做两级校验：
//!
//! 1. **纯 argv 级**（[`validate_invocation`]）：`GuardConfig.robot_id` ==
//!    [`identity::expected`]`(arm).robot_id`；s0 argv 的 `--robot-id` /
//!    `--hardware-id` 必须**存在**且与 D2 一致；`--config` 一律拒绝；
//! 2. **config 文件级**（[`GuardSteps::verify_identity`]）：按 D2 表逐字
//!    校验 `[robot].id` / `[[hardware]].id` / adapter / by-id 串号 /
//!    `motor_ids` / joints / `auto_enable=false`。
//!
//! 两级都是**配置级、不可自动恢复**错误 → 退 86，且**不消耗连败预算**。
//!
//! ## latch 路径单源
//!
//! provision 的目标路径**跟随 s0 args**：`--latch-path <p>` 在场即用它，
//! 否则回填 `/var/lib/robot/<robot_id>/estop_latch`（与 `s0_daemon` 的默认
//! 回填规则逐字一致）——保证 guard provision 的文件正是 daemon 将要读的
//! 那一份。
//!
//! **解析规则必须与 `s0_daemon::Args::parse` 同规则**（round 1 P0-3）：
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
use super::types::DeployArm;

/// 停止态退出码——**永久保留**（`s0_daemon` 任何路径不得以 86 退出，
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
    /// `s0_daemon` 可执行文件——必须 `is_absolute()` + `exists()`，违者退 86。
    pub s0_bin: PathBuf,
    /// systemd `RuntimeDirectory` 注入的计数目录。
    pub runtime_dir: PathBuf,
    /// 运行身份（计数 / 锁 / latch 路径均按它派生）。
    pub robot_id: String,
    /// `--` 之后透传给 `s0_daemon` 的参数。
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
    /// `s0_daemon` 的取值可能不同，一律拒绝（配置级错误 → 86）。
    pub fn resolve_latch_path(&self) -> Result<PathBuf, String> {
        match scan_opt(&self.s0_args, LATCH_PATH_OPT)? {
            Some(p) => Ok(PathBuf::from(p)),
            None => Ok(PathBuf::from(format!(
                "/var/lib/robot/{}/estop_latch",
                self.robot_id
            ))),
        }
    }
}

/// 按 **`s0_daemon::Args::parse` 的同一套 tokenize 规则**取某选项的唯一取值。
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
                "{opt}=<value> is not accepted by s0_daemon; use `{opt} <value>`"
            ));
        }
        if arg == OsStr::new(opt) {
            let value = args
                .get(i + 1)
                .ok_or_else(|| format!("{opt} requires a value"))?;
            if found.is_some() {
                return Err(format!(
                    "{opt} appears more than once (s0_daemon takes the last, guard the first — ambiguous)"
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
        }
    }

    /// 是否进入"停止自动重启"的停止态（退 86）。
    #[must_use]
    pub const fn is_stop_state(&self) -> bool {
        self.code() == GUARD_STOP_EXIT_CODE
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

    /// 本臂 latch provision。
    ///
    /// # Errors
    ///
    /// 见 [`super::provision::ProvisionError`]。
    fn provision(&mut self, cfg: &GuardConfig) -> Result<(), String>;

    /// `exec s0_daemon`（成功不返回）。
    fn exec_s0(&mut self, cfg: &GuardConfig, args: &[OsString]) -> ExecOutcome;
}

/// 拼 `s0_daemon` 的完整 argv：透传 `s0_args` 后**自动追加**
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
/// 单源可解析。
///
/// 不读 config 文件——文件级校验在 [`GuardSteps::verify_identity`]。
///
/// # Errors
///
/// 见 [`GuardExit::ExecFailed`]（全部是配置级、不可自动恢复 → 86）。
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

    // D2 身份：guard 自身的 robot_id 必须就是本臂的。
    let want = identity::expected(cfg.arm);
    if cfg.robot_id != want.robot_id {
        return Err(reject(
            "--robot-id",
            format!("expected {:?}, got {:?}", want.robot_id, cfg.robot_id),
        ));
    }

    // 转发给 s0_daemon 的身份必须存在且一致——否则 daemon 会以别的身份
    // （或 ROBOT_ID env / "default"）跑起来，topic / latch / 计数全错位。
    for (opt, expected_value) in [
        (ROBOT_ID_OPT, want.robot_id),
        (HARDWARE_ID_OPT, want.hardware_id),
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
    cfg.resolve_latch_path()
        .map_err(|e| reject(LATCH_PATH_OPT, e))?;
    Ok(())
}

/// 六步主流程。`Ok(())` 只在测试实现下可达（真实现 exec 成功不返回）。
///
/// # Errors
///
/// 见 [`GuardExit`]——每个变体都**绝不** exec `s0_daemon`。
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
    if let Err(reason) = steps.provision(cfg) {
        tracing::error!(arm = %cfg.arm, attempt, %reason, "guard: latch provision failed");
        return Err(pre_step_exit(attempt, PROVISION_FAIL_EXIT_CODE));
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

    use super::super::identity::load_and_verify_arm_config;
    use super::super::preflight::{ensure_safe_arm, feetech::open_arm_bus};
    use super::super::provision::provision_latch;
    use super::{GuardConfig, GuardSteps};

    /// 生产 [`GuardSteps`]。
    pub struct RealGuardSteps;

    impl GuardSteps for RealGuardSteps {
        fn verify_identity(&mut self, cfg: &GuardConfig) -> Result<(), String> {
            let verified =
                load_and_verify_arm_config(cfg.arm, &cfg.config_path).map_err(|e| e.to_string())?;
            // 与 validate_invocation 的 argv 侧合起来闭环：
            // argv 身份 == D2 == config 身份。
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
                "guard: D2 identity verified"
            );
            Ok(())
        }

        fn preflight(&mut self, cfg: &GuardConfig) -> Result<(), String> {
            let mut bus = open_arm_bus(cfg.arm, &cfg.config_path)?;
            ensure_safe_arm(cfg.arm, &mut bus).map_err(|report| report.to_string())
        }

        fn provision(&mut self, cfg: &GuardConfig) -> Result<(), String> {
            // validate_invocation 已保证可解析；这里再取一次保持单一来源。
            let path = cfg.resolve_latch_path()?;
            match provision_latch(&path) {
                Ok(outcome) => {
                    tracing::info!(latch = %path.display(), ?outcome, "guard: latch provisioned");
                    Ok(())
                }
                Err(e) => Err(format!("{}: {e}", path.display())),
            }
        }

        fn exec_s0(&mut self, cfg: &GuardConfig, args: &[OsString]) -> super::ExecOutcome {
            tracing::info!(
                s0_bin = %cfg.s0_bin.display(),
                arm = %cfg.arm,
                "guard: pre steps ok, exec s0_daemon"
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
        identity_result: Result<(), String>,
        preflight_result: Result<(), String>,
        provision_result: Result<(), String>,
        exec: ExecOutcome,
        calls: RefCell<Vec<&'static str>>,
        seen_count: RefCell<Option<u8>>,
        seen_args: RefCell<Vec<OsString>>,
    }

    impl ScriptedSteps {
        fn ok() -> Self {
            Self {
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
            let _ = cfg;
            self.calls.borrow_mut().push("verify_identity");
            self.identity_result.clone()
        }

        fn preflight(&mut self, cfg: &GuardConfig) -> Result<(), String> {
            self.calls.borrow_mut().push("preflight");
            *self.seen_count.borrow_mut() = read_count(&cfg.count_file()).ok();
            self.preflight_result.clone()
        }

        fn provision(&mut self, cfg: &GuardConfig) -> Result<(), String> {
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
        _dir: TempDir,
        cfg: GuardConfig,
    }

    fn fixture() -> Fixture {
        let dir = TempDir::new().unwrap();
        let s0_bin = dir.path().join("s0_daemon");
        std::fs::write(&s0_bin, b"#!/bin/true\n").unwrap();
        let runtime_dir = dir.path().join("run");
        std::fs::create_dir(&runtime_dir).unwrap();
        let cfg = GuardConfig {
            arm: DeployArm::Left,
            config_path: dir.path().join("xlerobot_left_arm.toml"),
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
        Fixture { _dir: dir, cfg }
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
            steps.provision_result = Err("latch broken".into());
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
        for bad in [PathBuf::from("s0_daemon"), f.cfg.runtime_dir.join("nope")] {
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
    /// `robot_id` 回填（与 `s0_daemon` 默认规则逐字一致）。
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

    /// **round 1 P0-3**：`s0_daemon` 对重复取最后一个、不认等号写法；guard
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

    /// **round 1 P0-1（argv 侧）**：转发给 `s0_daemon` 的身份必须存在且
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

    /// AC-E3 回归锁：**86 是 guard 永久保留码**——`s0_daemon` 任何路径都
    /// 不得以它退出，否则 unit 的 `RestartPreventExitStatus=86` 会连
    /// watchdog 重启一起吞掉（违反 §6.7 #2）。源级扫描 bin 里的
    /// `exit(<字面量>)` / `Err(<字面量>)`，并锁死其退出码表 = {0, 1, 2}。
    #[test]
    fn s0_daemon_never_exits_with_the_reserved_code() {
        const SRC: &str = include_str!("../bin/s0_daemon.rs");

        fn literal_codes(src: &str, marker: &str) -> Vec<u8> {
            src.match_indices(marker)
                .filter_map(|(i, _)| {
                    let rest = &src[i + marker.len()..];
                    let end = rest.find(')')?;
                    rest[..end].trim().parse::<u8>().ok()
                })
                .collect()
        }

        let mut codes = literal_codes(SRC, "exit(");
        codes.extend(literal_codes(SRC, "Err("));
        assert!(!codes.is_empty(), "扫描失效（marker 漂移）");
        for code in codes {
            assert_ne!(code, GUARD_STOP_EXIT_CODE, "s0_daemon 不得以 86 退出");
            assert!(code <= 2, "s0_daemon 退出码表 = 0/1/2，出现 {code}");
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
        // 86 不得与 s0_daemon(0/1/2) / S1(3/4/5) 既有码重叠。
        assert!(!(0..=5).contains(&GUARD_STOP_EXIT_CODE));
        assert_ne!(PREFLIGHT_FAIL_EXIT_CODE, GUARD_STOP_EXIT_CODE);
        assert_ne!(PROVISION_FAIL_EXIT_CODE, GUARD_STOP_EXIT_CODE);
    }
}
