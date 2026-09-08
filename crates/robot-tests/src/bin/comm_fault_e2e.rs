//! post-E 通信故障硬化独立进程验收入口。

#![cfg(target_os = "linux")]

#[path = "../comm_fault/mod.rs"]
mod comm_fault;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    comm_fault::main_entry().await
}
