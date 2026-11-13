//! 只读状态页入口；只连接本机gRPC，不创建S0/S1或访问设备。

use std::path::Path;

use robot_rpc::status_web::{StatusConfig, serve};

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    robot_logging::set_process_name("robot-status");
    let _ = robot_logging::init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.as_slice() == ["--help"] || args.as_slice() == ["-h"] {
        println!(
            "robot-status --config <状态配置.toml>\n只读HTTP/SSE状态页；不会启动机器人服务或发送控制命令。"
        );
        return;
    }
    if args.len() != 2 || args[0] != "--config" {
        eprintln!("用法：robot-status --config <状态配置.toml>");
        std::process::exit(2);
    }
    let config = match StatusConfig::load(Path::new(&args[1])) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("状态页配置无效：{error}");
            std::process::exit(2);
        }
    };
    let listener = match tokio::net::TcpListener::bind(config.listen_addr()).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("状态页监听失败：{error}");
            std::process::exit(1);
        }
    };
    tracing::info!(address = %config.listen_addr(), "只读状态页开始监听");
    if let Err(error) = serve(listener, config, shutdown_signal()).await {
        eprintln!("状态页退出失败：{error}");
        std::process::exit(1);
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {},
                    _ = terminate.recv() => {},
                }
            }
            Err(error) => tracing::error!(%error, "无法安装退出信号监听，立即收尾"),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
