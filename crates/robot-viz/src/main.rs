//! 独立只读观测工具；真实 SDK 在可回收的专用子进程中运行。

use robot_logging as _;
use robot_viz::{runtime, supervisor};

#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.is_empty() || args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!(
            "robot-viz --config <机器人.toml> [--config <另一机器人.toml>]\n  (--save <输出.rrd> | --connect <rerun+http://127.0.0.1:9876/proxy>)\n  [--duration-ms <毫秒>] [--max-frames <帧数>] [--report <报告.json>]\n\n仅观察隔离镜像，不启动 S0/S1、不访问串口、不发控制命令。\nconnect 默认持续到 Ctrl-C；save 默认 10 秒/1000 帧，单文件最多 10000 帧。\n不支持的数据和未知布局明确拒绝；录制成功不表示当前物理安全。"
        );
        return Ok(());
    }
    let options = runtime::Options::parse(args.clone())?;
    supervisor::install_signals();
    if std::env::var_os("ROBOT_VIZ_INTERNAL_WORKER").as_deref() == Some(std::ffi::OsStr::new("1")) {
        runtime::run(&options)
    } else {
        supervisor::run(&args, options.report.as_deref())
    }
}
