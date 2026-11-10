//! post-E 通信故障硬化独立进程验收入口。

#![cfg(target_os = "linux")]

#[path = "../comm_fault/mod.rs"]
mod comm_fault;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    robot_logging::set_process_name("robot-comm-e2e");
    comm_fault::main_entry().await
}
