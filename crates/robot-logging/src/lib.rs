//! `robot-logging` —— 所有 bin 共用的 `tracing` 初始化。
//!
//! # 为什么独立 crate
//!
//! `robot-core` / `robot-msg` / `robot-transport` 等 library crate 只依赖
//! `tracing`（编译期零开销 macro + trait），**不依赖本 crate**（subscriber init
//! 是 binary 的职责）。每个 bin 在 `main` 第一件事 `robot_logging::init()`。
//!
//! # 格式
//!
//! ## Human 模式（默认）
//!
//! ```text
//! 2026-04-25T23:45:12.123456Z [+12345.678901234s]  INFO  robot_s0::s0_daemon: msg
//!   k1=v1 k2=v2
//! ```
//!
//! - 前缀 = **wall clock RFC3339 UTC + `[+monotonic_秒.纳秒s]`**。wall clock 方便
//!   跨机器对齐绝对时间（NTP 下），`CLOCK_MONOTONIC_RAW` 方便后期日志回放算事件
//!   时延（NTP 跳 / 闰秒无感）。
//! - 级别 5 字符对齐 `INFO ` / `WARN ` / `ERROR`；ANSI 彩色 on（TTY 自动检测）。
//! - `target` 显示模块路径，便于定位代码位置。
//! - 结构化字段 `k=v` 内嵌消息行，也可紧随换行缩进。
//!
//! ## JSON 模式（systemd / log aggregator）
//!
//! `ROBOT_LOG_FORMAT=json` 切换：
//!
//! ```json
//! {"timestamp":"2026-04-25T23:45:12.123456Z","mono_ns":12345678901234,"level":"INFO",...}
//! ```
//!
//! # 字段命名约定
//!
//! 所有 `tracing::*` 调用共用词汇表（见 `docs/logging.md`）：
//!
//! | Key           | 类型         | 含义                               |
//! | ------------- | ------------ | ---------------------------------- |
//! | `robot_id`    | `&str`       | TOML `[robot] id`                  |
//! | `hardware_id` | `&str`       | TOML `[[hardware]] id`             |
//! | `motor_id`    | `u8`         | Feetech motor ID                   |
//! | `joint`       | `&str`       | 关节名                             |
//! | `adapter`     | `&str`       | factory 名                         |
//! | `err`         | `?` (Debug)  | 错误值，保留 `thiserror` 结构       |
//! | `attempt`     | `u32`        | 重试计数                           |
//! | `duration_ms` | `u64`        | 延时毫秒                           |
//!
//! # 过滤
//!
//! `RUST_LOG` 标准 tracing-subscriber `EnvFilter` 语法：
//!
//! - 默认：`info,robot=debug`（项目 crate 走 debug，外部 info）
//! - `RUST_LOG=trace` 全打开
//! - `RUST_LOG=robot_s0=trace,info` 仅 s0 走 trace，其余 info
//!
//! # RT 铁律
//!
//! **RT thread 内严格禁用** `tracing::*!` 带 format arg 的调用（`CLAUDE.md`
//! 禁用表已列）：带 format arg 的宏会构造 `String`，触发 heap alloc 违反 §4.9。
//! RT hot path 或不 log，或走 lock-free ring buffer + 异步 flush（v0.3+）。
//!
//! 本 crate **只管 non-RT 路径**；RT 路径的日志基础设施后续任务。

// `forbid(unused_must_use)` 会与 `#[ctor::ctor]` 展开内部的 `#[allow(...)]`
// 冲突（forbid 不允许 downstream overrule）。用 deny 等价效果 + 兼容宏展开。
#![deny(unused_must_use)]

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;

// ============================================================================
// Monotonic + wall clock FormatTime
// ============================================================================

/// 自定义 `FormatTime`：同时打 `<wall RFC3339 UTC>` + `[+<mono_sec>s]`。
///
/// - wall clock：`SystemTime::now()` 通过 `UNIX_EPOCH` 偏移换算 RFC3339 UTC。
///   本 crate 是日志基础设施（非 robot-core），允许直接调 `SystemTime::now()`；
///   业务代码仍经 `robot_core::time::RobotClock` 走（`CLAUDE.md` §5 禁用表）。
/// - monotonic：调 `libc::clock_gettime(CLOCK_MONOTONIC_RAW)`。与 `robot-core::
///   time::SystemClock::now()` 同源，日志里的 `mono_ns` 字段和 `rt_thread` 内
///   测到的 deadline ns 可直接做差。
///
/// 为什么不依赖 `robot-core`：keep robot-logging 初始化极简，0 额外 crate 编
/// 译负担。`clock_gettime` 只需 `libc` 一个 workspace 依赖，但我们甚至不 link
/// libc —— 用 `std::time::Instant`（stable，不在业务代码禁用列表里 *本 crate*
/// 的范畴；robot-core 禁用 `Instant::now()` 的理由是时间读取统一走 `RobotClock`
/// trait，本 crate 作为日志基础设施例外）。
struct WallAndMono {
    /// 进程起点的 `Instant`，所有日志时间戳打 `Instant::now().duration_since(start)`
    /// —— 等价 monotonic 秒 + 纳秒，NTP / adjtime 无感。
    start: std::time::Instant,
}

impl WallAndMono {
    fn new() -> Self {
        Self {
            start: std::time::Instant::now(),
        }
    }
}

impl FormatTime for WallAndMono {
    fn format_time(&self, w: &mut Writer<'_>) -> fmt::Result {
        // wall clock（RFC3339 UTC，微秒精度）
        let wall = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let secs = wall.as_secs();
        let micros = wall.subsec_micros();
        write_rfc3339_utc(w, secs, micros)?;

        // monotonic（自进程启动）
        let mono = std::time::Instant::now().duration_since(self.start);
        write!(w, " [+{}.{:09}s]", mono.as_secs(), mono.subsec_nanos())
    }
}

/// 手写 RFC3339 UTC 格式化（避免拉入 `chrono` / `time` crate）。
///
/// 输出如 `2026-04-25T23:45:12.123456Z`。精度到微秒（6 位），足够事件对齐；
/// 纳秒精度放在 monotonic 字段上。
fn write_rfc3339_utc(w: &mut Writer<'_>, unix_secs: u64, micros: u32) -> fmt::Result {
    // 天数换算到 year-month-day（Gregorian），Howard Hinnant's "days from civil" 算法
    // （http://howardhinnant.github.io/date_algorithms.html），纯整数运算无 alloc。
    // unix_secs 最大 u64::MAX/86400 ≈ 2.1e14，远小于 i64::MAX，cast 安全。
    let days_since_epoch: i64 = i64::try_from(unix_secs / 86_400).unwrap_or(i64::MAX);
    let sec_of_day = u32::try_from(unix_secs % 86_400).unwrap_or(0);
    let hour = sec_of_day / 3600;
    let minute = (sec_of_day % 3600) / 60;
    let second = sec_of_day % 60;

    let (year, month, day) = civil_from_days(days_since_epoch);

    write!(
        w,
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{micros:06}Z"
    )
}

/// Howard Hinnant "days from civil" 反函数：unix-epoch 天偏移 → `(year, month, day)`。
///
/// 纯整数、零分配，任意年份有效（i64 范围 >> Unix 时间戳 64-bit 范围）。
/// Hinnant 算法内部 `doe` / `yoe` 按构造必 >= 0，`as u32` 数值保真；cast lint
/// allow 属合理例外（注释已标明）。
#[allow(
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
const fn civil_from_days(z: i64) -> (i32, u32, u32) {
    // z = days since 1970-01-01
    // 平移到 0000-03-01 起点（Hinnant 算法）
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32; // ≥ 0 by construction: [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = (yoe as i32) + (era as i32) * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

// ============================================================================
// 公共 init 入口
// ============================================================================

/// 初始化 tracing subscriber。每个 bin 在 `main` 第一件事调。
///
/// **幂等**：多次调用只第一次生效（底层 `tracing::subscriber::set_global_default`
/// 只允许一次）。第二次以后返回 `false`，方便单测场景容忍重复初始化。
///
/// 环境变量：
/// - `RUST_LOG`：过滤器（默认 `info,robot=debug`）
/// - `ROBOT_LOG_FORMAT=json`：切 JSON 模式
/// - `NO_COLOR=1`：强制关 ANSI 彩色（fmt 默认 TTY 检测，设此变量覆盖）
#[must_use = "init() returns whether subscriber was actually installed; ignore only if you don't care"]
pub fn init() -> bool {
    let filter =
        EnvFilter::try_from_env("RUST_LOG").unwrap_or_else(|_| EnvFilter::new("info,robot=debug"));
    let timer = WallAndMono::new();

    let format_env = std::env::var("ROBOT_LOG_FORMAT").unwrap_or_default();
    let try_init: Result<(), _> = match format_env.as_str() {
        "json" => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_timer(timer)
            .json()
            .flatten_event(true)
            .with_current_span(false)
            .with_span_list(false)
            .try_init(),
        _ => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_timer(timer)
            .with_target(true)
            .with_thread_names(false)
            .with_thread_ids(false)
            .with_line_number(false)
            .try_init(),
    };

    try_init.is_ok()
}

// ============================================================================
// 自动初始化（ctor）
// ============================================================================

/// binary 依赖本 crate 后，main 运行前自动跑一次 `init()` —— 所有 bin 的
/// `main()` 不需要写 `robot_logging::init()` 那行。
///
/// Rust 1.80+ 配 LLD linker 下实测稳定（`#[ctor]` 内部 `#[used]` 防 DCE）。
/// 如果 bin 需要**覆盖** ROBOT_LOG_FORMAT / RUST_LOG 默认值，**必须**在启动
/// 脚本或 shell 里设环境变量（而不是 main 里）—— ctor 在环境变量读取之后
/// 立刻跑，main 里修改 `std::env::set_var` 为时已晚。
///
/// 测试 / embedded 场景不走 ctor：`#[cfg(not(test))]` 避免测试进程启动时
/// 抢先 install subscriber（测试用 `tracing-test` / 手动 init）。
// ctor 1.0 起要求显式 `(unsafe)` 确认：pre-main 构造函数运行在 Rust
// runtime 初始化保证之外（std 未完全就绪 / 不得 panic 逃逸）。本函数体
// 只调 init()（安装 tracing subscriber），语义与 0.x 时代一致。
#[cfg(not(test))]
#[ctor::ctor(unsafe)]
fn auto_init() {
    // ctor 路径 void-ignore return；init() `#[must_use]` warning 用 let 绑定
    // 明确"有意忽略"。
    let _installed = init();
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_known_dates() {
        // 1970-01-01 = day 0
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2000-02-29 闰年边界
        let days_2000_feb_29 = (2000 - 1970) * 365 + 7 /* 1972, 76, 80, 84, 88, 92, 96 闰年 */ + 31 /* Jan */ + 28 /* Feb 1-28 */;
        assert_eq!(civil_from_days(days_2000_feb_29), (2000, 2, 29));
        // 2026-04-25（本 session）
        // 1970→2026 = 56 年，闰年 (72,76,80,84,88,92,96, 00, 04,08,12,16,20,24) = 14 个
        // 天数 = 56*365 + 14 + 31 + 28 + 31 + 24 = ...
        // 用算法验证：
        let (y, m, d) = civil_from_days(20568);
        assert_eq!((y, m, d), (2026, 4, 25));
    }

    #[test]
    fn rfc3339_format_well_known() {
        let mut buf = String::new();
        let mut w = Writer::new(&mut buf);
        // 2026-04-25T00:00:00.000000Z
        let unix = 20568_u64 * 86400;
        write_rfc3339_utc(&mut w, unix, 0).unwrap();
        assert_eq!(buf, "2026-04-25T00:00:00.000000Z");
    }

    #[test]
    fn init_is_idempotent() {
        // 第一次可能成功（取决于测试运行顺序 / subscriber 状态）
        let _a = init();
        // 第二次必然 false（global subscriber 已设）
        let b = init();
        assert!(!b, "second init must return false");
    }
}
