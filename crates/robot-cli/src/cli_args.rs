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
    /// 非 flag 位置参数（clear-estop / clear-error 无；handler 校验为空）。
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

#[cfg(test)]
mod tests {
    use super::{
        ArgError, DEFAULT_TIMEOUT_MS, expect_one_positional, parse_gateway_args,
        reject_confirmation, reject_positional,
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
        assert!(a.positional.is_empty());
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
        assert!(a.positional.is_empty());
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
}
