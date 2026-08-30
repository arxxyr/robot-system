//! gateway 子命令**手写**参数 parser（Plan round 1 P1-2：不引 clap）。
//!
//! 既有 `status` / `watch` / `estop` 的手写 parser 在 `bin/robotctl.rs`
//! （`Args::parse`），跟本模块**互不影响**：入口分流后，async-lane 子命令
//! 才走这里。共享 flag 一处解析，各 commit 增量扩 positional / 专属 flag。

use std::fmt;

/// `--timeout-ms` 默认值（ms；AC-6：默认 2000）。
pub const DEFAULT_TIMEOUT_MS: u64 = 2000;

/// gateway 子命令共享参数。候选 A (2/5) = clear-estop / clear-error 用；
/// `positional` / invoke 专属 flag 随后续 commit 扩。
#[derive(Debug, PartialEq, Eq)]
pub struct GatewayArgs {
    /// `--gateway-addr`（None = 走 env `ROBOT_GATEWAY_ADDR` / 默认）。
    pub gateway_addr: Option<String>,
    /// `--token`（None = 走 env `ROBOT_TOKEN`）。
    pub token: Option<String>,
    /// `--source`（None = [`crate::meta::default_cli_source`]）。
    pub source: Option<String>,
    /// `--idempotency-key`（None / 空 = 自动 `cli-<request_id>`）。
    pub idempotency_key: Option<String>,
    /// `--timeout-ms`（默认 [`DEFAULT_TIMEOUT_MS`]）。
    pub timeout_ms: u64,
    /// `--dry-run`。
    pub dry_run: bool,
    /// `--json`。
    pub json: bool,
    /// `--audit-reason`（写 RPC 必填；gate 校验）。
    pub audit_reason: Option<String>,
    /// `--confirmation`（clear-estop 专属；AC-3 四道闸）。
    pub confirmation: Option<String>,
    /// `--waypoints-file <PATH>`（invoke scripted 专属；AC-8）。
    pub waypoints_file: Option<String>,
    /// `--follow`（invoke 专属；默认 off，Q-CLI11(b)）。
    pub follow: bool,
    /// `--target-x`（invoke ik 专属；原样字符串，handler parse f64）。
    pub target_x: Option<String>,
    /// `--target-y`（invoke ik 专属）。
    pub target_y: Option<String>,
    /// `--target-z`（invoke ik 专属）。
    pub target_z: Option<String>,
    /// `--target-qx`（invoke ik 专属；默认 0）。
    pub target_qx: Option<String>,
    /// `--target-qy`（invoke ik 专属；默认 0）。
    pub target_qy: Option<String>,
    /// `--target-qz`（invoke ik 专属；默认 0）。
    pub target_qz: Option<String>,
    /// `--target-qw`（invoke ik 专属；默认 1）。
    pub target_qw: Option<String>,
    /// `--seed-joints`（invoke ik 专属；逗号列表）。
    pub seed_joints: Option<String>,
    /// `--frame`（invoke ik 专属）。
    pub frame: Option<String>,
    /// 非 flag 位置参数（clear-estop / clear-error 无；TaskHandleRef 族取一个
    /// action-id；invoke 取 kind 子类型 scripted/ik）。
    pub positional: Vec<String>,
}

impl Default for GatewayArgs {
    fn default() -> Self {
        Self {
            gateway_addr: None,
            token: None,
            source: None,
            idempotency_key: None,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            dry_run: false,
            json: false,
            audit_reason: None,
            confirmation: None,
            waypoints_file: None,
            follow: false,
            target_x: None,
            target_y: None,
            target_z: None,
            target_qx: None,
            target_qy: None,
            target_qz: None,
            target_qw: None,
            seed_joints: None,
            frame: None,
            positional: Vec::new(),
        }
    }
}

/// gateway 子命令参数解析失败 / `--help` 请求。`Display` 不含 `error: `
/// 前缀（binary 打印时统一加；`Help` 不走错误打印）。
#[derive(Debug, PartialEq, Eq)]
pub enum ArgError {
    /// flag 缺值（如 `--token` 出现在末尾）。
    MissingValue(&'static str),
    /// `--timeout-ms` 不是合法非负整数。
    BadTimeout(String),
    /// 未知 flag / arg。
    Unknown(String),
    /// `-h` / `--help`（非错误；handler 打 usage + exit 0）。
    Help,
}

impl fmt::Display for ArgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingValue(flag) => write!(f, "{flag} requires a value"),
            Self::BadTimeout(v) => {
                write!(f, "--timeout-ms must be a non-negative integer, got: {v}")
            }
            Self::Unknown(a) => write!(f, "unknown argument: {a}"),
            Self::Help => write!(f, "help requested"),
        }
    }
}

/// 取 flag 的值（`raw[*i]` 是 flag 名；推进到值并返回；缺值报错）。
fn take_value(raw: &[String], i: &mut usize, flag: &'static str) -> Result<String, ArgError> {
    *i += 1;
    raw.get(*i).cloned().ok_or(ArgError::MissingValue(flag))
}

/// 解析 gateway 子命令参数（`raw` = 子命令名**之后**的全部 arg）。
///
/// 识别共享 flag + clear-estop 专属 `--confirmation`；非 flag token 收进
/// `positional`。`-h` / `--help` 立即短路返 [`ArgError::Help`]。
///
/// # Errors
///
/// flag 缺值（[`ArgError::MissingValue`]）/ `--timeout-ms` 非整数
/// （[`ArgError::BadTimeout`]）/ 未知 flag（[`ArgError::Unknown`]）/
/// `--help`（[`ArgError::Help`]）。
pub fn parse_gateway_args(raw: &[String]) -> Result<GatewayArgs, ArgError> {
    let mut args = GatewayArgs::default();
    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "--gateway-addr" => {
                args.gateway_addr = Some(take_value(raw, &mut i, "--gateway-addr")?);
            }
            "--token" => args.token = Some(take_value(raw, &mut i, "--token")?),
            "--source" => args.source = Some(take_value(raw, &mut i, "--source")?),
            "--idempotency-key" => {
                args.idempotency_key = Some(take_value(raw, &mut i, "--idempotency-key")?);
            }
            "--audit-reason" => {
                args.audit_reason = Some(take_value(raw, &mut i, "--audit-reason")?);
            }
            "--confirmation" => {
                args.confirmation = Some(take_value(raw, &mut i, "--confirmation")?);
            }
            "--timeout-ms" => {
                let v = take_value(raw, &mut i, "--timeout-ms")?;
                args.timeout_ms = v.parse().map_err(|_| ArgError::BadTimeout(v))?;
            }
            "--dry-run" => args.dry_run = true,
            "--json" => args.json = true,
            // invoke 专属 flag（(4/5)）；各子命令由 reject_invoke_flags_except
            // 按白名单拒（不静默吞，沿阻断 1）。
            "--waypoints-file" => {
                args.waypoints_file = Some(take_value(raw, &mut i, "--waypoints-file")?);
            }
            "--follow" => args.follow = true,
            "--target-x" => args.target_x = Some(take_value(raw, &mut i, "--target-x")?),
            "--target-y" => args.target_y = Some(take_value(raw, &mut i, "--target-y")?),
            "--target-z" => args.target_z = Some(take_value(raw, &mut i, "--target-z")?),
            "--target-qx" => args.target_qx = Some(take_value(raw, &mut i, "--target-qx")?),
            "--target-qy" => args.target_qy = Some(take_value(raw, &mut i, "--target-qy")?),
            "--target-qz" => args.target_qz = Some(take_value(raw, &mut i, "--target-qz")?),
            "--target-qw" => args.target_qw = Some(take_value(raw, &mut i, "--target-qw")?),
            "--seed-joints" => args.seed_joints = Some(take_value(raw, &mut i, "--seed-joints")?),
            "--frame" => args.frame = Some(take_value(raw, &mut i, "--frame")?),
            "-h" | "--help" => return Err(ArgError::Help),
            other if other.starts_with('-') => return Err(ArgError::Unknown(other.to_owned())),
            other => args.positional.push(other.to_owned()),
        }
        i += 1;
    }
    Ok(args)
}

/// 子命令不接受位置参数时的早拒（clear-estop / clear-error）。共享 parser
/// 会把非 flag token 收进 `positional`；不收 positional 的子命令用本函数拒。
///
/// # Errors
///
/// `args.positional` 非空。
pub fn reject_positional(subcommand: &str, args: &GatewayArgs) -> Result<(), String> {
    if args.positional.is_empty() {
        Ok(())
    } else {
        Err(format!("{subcommand} takes no positional arguments"))
    }
}

/// 子命令不接受 clear-estop 专属的 `--confirmation` 时的早拒。共享 parser 会
/// 收下 `--confirmation`（clear-estop 需要它）；其它子命令（clear-error 及
/// 后续非 clear-estop 写 RPC）用本函数拒，**阻断"静默吞 --confirmation"**。
///
/// # Errors
///
/// `args.confirmation` 为 `Some`（无论值是否为合法 literal）。
pub fn reject_confirmation(subcommand: &str, args: &GatewayArgs) -> Result<(), String> {
    if args.confirmation.is_none() {
        Ok(())
    } else {
        Err(format!("--confirmation is not valid for {subcommand}"))
    }
}

/// 提取**恰好一个**位置参数（`<action-id>`：get-task-status / cancel-task /
/// stream-events 各取一个 action-id）。值合法性（UUIDv7）由
/// [`crate::validate::validate_action_id`] 另校验（AC-11）。
///
/// # Errors
///
/// 0 个 → `<subcommand> requires <action-id>`；>1 个 →
/// `<subcommand> takes exactly one <action-id>`。
pub fn expect_one_positional(subcommand: &str, args: &GatewayArgs) -> Result<String, String> {
    match args.positional.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(format!("{subcommand} requires <action-id>")),
        _ => Err(format!("{subcommand} takes exactly one <action-id>")),
    }
}

/// 拒绝不在 `allowed` 白名单内的 invoke-family flag（沿阻断 1：**不静默吞**）。
/// invoke-family = {`--waypoints-file`, `--follow`, `--target-*`,
/// `--seed-joints`, `--frame`}。共享 parser 收下它们；各子命令按白名单拒：
/// 非 invoke 子命令 `allowed = []`（全拒）；`invoke scripted` 放
/// `--waypoints-file` / `--follow`；`invoke ik` 放 `--target-*` /
/// `--seed-joints` / `--frame` / `--follow`。
///
/// # Errors
///
/// 任一被设置的 invoke-family flag 不在 `allowed` 内（报第一个命中的）。
pub fn reject_invoke_flags_except(
    subcommand: &str,
    args: &GatewayArgs,
    allowed: &[&str],
) -> Result<(), String> {
    let invoke_flags: [(bool, &str); 11] = [
        (args.waypoints_file.is_some(), "--waypoints-file"),
        (args.follow, "--follow"),
        (args.target_x.is_some(), "--target-x"),
        (args.target_y.is_some(), "--target-y"),
        (args.target_z.is_some(), "--target-z"),
        (args.target_qx.is_some(), "--target-qx"),
        (args.target_qy.is_some(), "--target-qy"),
        (args.target_qz.is_some(), "--target-qz"),
        (args.target_qw.is_some(), "--target-qw"),
        (args.seed_joints.is_some(), "--seed-joints"),
        (args.frame.is_some(), "--frame"),
    ];
    for (set, name) in invoke_flags {
        if set && !allowed.contains(&name) {
            return Err(format!("{name} is not valid for {subcommand}"));
        }
    }
    Ok(())
}

/// 拒绝 wire **不支持**的 `--dry-run`。当前仅 `clear-estop` 端到端支持
/// `dry_run`（`clear_estop.fbs` 带 `dry_run` 字段 + S0 action layer 真识别）；
/// 其它子命令的 wire fbs（`InvokeTaskRequest` / `CancelTaskRequest` /
/// `ClearErrorRequest`）**不带** `dry_run`，`TaskHandleRef` 读 RPC 更无 meta ——
/// 若接受 `--dry-run`，operator 会误以为 dry-run 生效但实发真实动作（危险）。
/// 真正支持需 schema + daemon 语义扩展（v0.5+），不在候选 A。
///
/// # Errors
///
/// `args.dry_run` 为 true。
pub fn reject_dry_run(subcommand: &str, args: &GatewayArgs) -> Result<(), String> {
    if args.dry_run {
        Err(format!(
            "--dry-run is not supported for {subcommand} (wire schema carries dry_run only for clear-estop)"
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ArgError, DEFAULT_TIMEOUT_MS, expect_one_positional, parse_gateway_args,
        reject_confirmation, reject_dry_run, reject_invoke_flags_except, reject_positional,
    };

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn empty_yields_defaults() {
        let a = parse_gateway_args(&[]).expect("parse");
        assert_eq!(a.timeout_ms, DEFAULT_TIMEOUT_MS);
        assert!(!a.dry_run);
        assert!(!a.json);
        assert!(a.gateway_addr.is_none());
        assert!(a.token.is_none());
        assert!(a.source.is_none());
        assert!(a.idempotency_key.is_none());
        assert!(a.audit_reason.is_none());
        assert!(a.confirmation.is_none());
        assert!(
            a.positional.is_empty(),
            "expected no positionals, got {:?}",
            a.positional
        );
    }

    #[test]
    fn parses_all_common_flags() {
        let a = parse_gateway_args(&v(&[
            "--gateway-addr",
            "1.2.3.4:5",
            "--token",
            "tok",
            "--source",
            "cli.x",
            "--idempotency-key",
            "k1",
            "--timeout-ms",
            "1500",
            "--audit-reason",
            "why",
            "--confirmation",
            "CONFIRM_CLEAR_ESTOP",
            "--dry-run",
            "--json",
        ]))
        .expect("parse");
        assert_eq!(a.gateway_addr.as_deref(), Some("1.2.3.4:5"));
        assert_eq!(a.token.as_deref(), Some("tok"));
        assert_eq!(a.source.as_deref(), Some("cli.x"));
        assert_eq!(a.idempotency_key.as_deref(), Some("k1"));
        assert_eq!(a.timeout_ms, 1500);
        assert_eq!(a.audit_reason.as_deref(), Some("why"));
        assert_eq!(a.confirmation.as_deref(), Some("CONFIRM_CLEAR_ESTOP"));
        assert!(a.dry_run);
        assert!(a.json);
        assert!(
            a.positional.is_empty(),
            "expected no positionals, got {:?}",
            a.positional
        );
    }

    #[test]
    fn missing_value_errors() {
        assert_eq!(
            parse_gateway_args(&v(&["--token"])),
            Err(ArgError::MissingValue("--token"))
        );
        assert_eq!(
            parse_gateway_args(&v(&["--audit-reason"])),
            Err(ArgError::MissingValue("--audit-reason"))
        );
        assert_eq!(
            parse_gateway_args(&v(&["--timeout-ms"])),
            Err(ArgError::MissingValue("--timeout-ms"))
        );
    }

    #[test]
    fn bad_timeout_errors() {
        assert_eq!(
            parse_gateway_args(&v(&["--timeout-ms", "abc"])),
            Err(ArgError::BadTimeout("abc".to_owned()))
        );
        // 负数非法（u64 parse fail）。
        assert!(matches!(
            parse_gateway_args(&v(&["--timeout-ms", "-5"])),
            Err(ArgError::BadTimeout(_))
        ));
    }

    #[test]
    fn timeout_zero_is_valid() {
        let a = parse_gateway_args(&v(&["--timeout-ms", "0"])).expect("parse");
        assert_eq!(a.timeout_ms, 0);
    }

    #[test]
    fn unknown_flag_errors() {
        assert_eq!(
            parse_gateway_args(&v(&["--bogus"])),
            Err(ArgError::Unknown("--bogus".to_owned()))
        );
        assert_eq!(
            parse_gateway_args(&v(&["-x"])),
            Err(ArgError::Unknown("-x".to_owned()))
        );
    }

    #[test]
    fn help_flag_short_circuits() {
        assert_eq!(parse_gateway_args(&v(&["--help"])), Err(ArgError::Help));
        assert_eq!(parse_gateway_args(&v(&["-h"])), Err(ArgError::Help));
        assert_eq!(
            parse_gateway_args(&v(&["--token", "t", "--help"])),
            Err(ArgError::Help)
        );
    }

    #[test]
    fn positionals_collected() {
        let a = parse_gateway_args(&v(&["abc", "--json", "def"])).expect("parse");
        assert_eq!(a.positional, vec!["abc".to_owned(), "def".to_owned()]);
        assert!(a.json);
    }

    #[test]
    fn reject_positional_ok_when_empty() {
        let a = parse_gateway_args(&v(&["--json"])).expect("parse");
        assert_eq!(reject_positional("clear-estop", &a), Ok(()));
    }

    #[test]
    fn reject_positional_errs_when_present() {
        let a = parse_gateway_args(&v(&["extra"])).expect("parse");
        assert_eq!(
            reject_positional("clear-error", &a),
            Err("clear-error takes no positional arguments".to_owned())
        );
    }

    #[test]
    fn reject_confirmation_ok_when_absent() {
        let a = parse_gateway_args(&v(&["--token", "t"])).expect("parse");
        assert_eq!(reject_confirmation("clear-error", &a), Ok(()));
    }

    /// 阻断 1：clear-error 带 --confirmation（即使值合法）必须拒，不静默吞。
    #[test]
    fn reject_confirmation_errs_even_for_valid_literal() {
        let a = parse_gateway_args(&v(&["--confirmation", "CONFIRM_CLEAR_ESTOP"])).expect("parse");
        assert_eq!(
            reject_confirmation("clear-error", &a),
            Err("--confirmation is not valid for clear-error".to_owned())
        );
    }

    #[test]
    fn expect_one_positional_ok() {
        let a = parse_gateway_args(&v(&["the-id"])).expect("parse");
        assert_eq!(
            expect_one_positional("get-task-status", &a),
            Ok("the-id".to_owned())
        );
    }

    #[test]
    fn expect_one_positional_missing() {
        let a = parse_gateway_args(&v(&["--json"])).expect("parse");
        assert_eq!(
            expect_one_positional("cancel-task", &a),
            Err("cancel-task requires <action-id>".to_owned())
        );
    }

    #[test]
    fn expect_one_positional_too_many() {
        let a = parse_gateway_args(&v(&["a", "b"])).expect("parse");
        assert_eq!(
            expect_one_positional("stream-events", &a),
            Err("stream-events takes exactly one <action-id>".to_owned())
        );
    }

    #[test]
    fn parses_invoke_flags() {
        let a = parse_gateway_args(&v(&[
            "--waypoints-file",
            "/tmp/wp.json",
            "--follow",
            "--target-x",
            "0.2",
            "--target-y",
            "0.0",
            "--target-z",
            "0.1",
            "--target-qw",
            "1.0",
            "--seed-joints",
            "0.1,0.2",
            "--frame",
            "base",
        ]))
        .expect("parse");
        assert_eq!(a.waypoints_file.as_deref(), Some("/tmp/wp.json"));
        assert!(a.follow);
        assert_eq!(a.target_x.as_deref(), Some("0.2"));
        assert_eq!(a.target_z.as_deref(), Some("0.1"));
        assert_eq!(a.target_qw.as_deref(), Some("1.0"));
        assert_eq!(a.seed_joints.as_deref(), Some("0.1,0.2"));
        assert_eq!(a.frame.as_deref(), Some("base"));
    }

    #[test]
    fn reject_invoke_flags_non_invoke_allows_none() {
        let a = parse_gateway_args(&v(&["--token", "t"])).expect("parse");
        assert_eq!(reject_invoke_flags_except("clear-estop", &a, &[]), Ok(()));
    }

    /// 非 invoke 子命令带 invoke flag 必须拒（allowed=[]，不静默吞，沿阻断 1）。
    #[test]
    fn reject_invoke_flags_non_invoke_errs() {
        let a = parse_gateway_args(&v(&["--waypoints-file", "/x"])).expect("parse");
        assert_eq!(
            reject_invoke_flags_except("clear-estop", &a, &[]),
            Err("--waypoints-file is not valid for clear-estop".to_owned())
        );
        let b = parse_gateway_args(&v(&["--follow"])).expect("parse");
        assert_eq!(
            reject_invoke_flags_except("get-task-status", &b, &[]),
            Err("--follow is not valid for get-task-status".to_owned())
        );
    }

    /// invoke scripted 放 --waypoints-file / --follow，但拒 ik flag。
    #[test]
    fn reject_invoke_flags_scripted_whitelist() {
        let a = parse_gateway_args(&v(&["--waypoints-file", "/x", "--follow"])).expect("parse");
        assert_eq!(
            reject_invoke_flags_except("invoke scripted", &a, &["--waypoints-file", "--follow"]),
            Ok(())
        );
        let b = parse_gateway_args(&v(&["--target-x", "0.1"])).expect("parse");
        assert_eq!(
            reject_invoke_flags_except("invoke scripted", &b, &["--waypoints-file", "--follow"]),
            Err("--target-x is not valid for invoke scripted".to_owned())
        );
    }

    #[test]
    fn reject_dry_run_ok_when_absent() {
        let a = parse_gateway_args(&v(&["--token", "t"])).expect("parse");
        assert_eq!(reject_dry_run("clear-error", &a), Ok(()));
    }

    /// wire 不支持 `dry_run` 的子命令带 --dry-run 必须拒（防误导危险动作）。
    #[test]
    fn reject_dry_run_errs_when_set() {
        let a = parse_gateway_args(&v(&["--dry-run"])).expect("parse");
        assert_eq!(
            reject_dry_run("invoke scripted", &a),
            Err("--dry-run is not supported for invoke scripted (wire schema carries dry_run only for clear-estop)".to_owned())
        );
    }

    /// invoke ik 放 --target-* / --follow，但拒 --waypoints-file。
    #[test]
    fn reject_invoke_flags_ik_whitelist() {
        let allowed = [
            "--target-x",
            "--target-y",
            "--target-z",
            "--target-qx",
            "--target-qy",
            "--target-qz",
            "--target-qw",
            "--seed-joints",
            "--frame",
            "--follow",
        ];
        let a = parse_gateway_args(&v(&["--target-x", "0.1", "--frame", "base"])).expect("parse");
        assert_eq!(
            reject_invoke_flags_except("invoke ik", &a, &allowed),
            Ok(())
        );
        let b = parse_gateway_args(&v(&["--waypoints-file", "/x"])).expect("parse");
        assert_eq!(
            reject_invoke_flags_except("invoke ik", &b, &allowed),
            Err("--waypoints-file is not valid for invoke ik".to_owned())
        );
    }
}
