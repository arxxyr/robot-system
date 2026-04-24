//! # robot-cli
//!
//! `robotctl` —— 最小可观测运维 CLI。v0.2 Part 2 scope：
//! - `robotctl status`：订阅一次 S0 `JointState` + `Heartbeat`，打印后退出。
//! - `robotctl watch`：每 100 ms 刷一次。
//!
//! 未来（Part 3+）扩展：
//! - `robotctl estop` / `robotctl clear-estop` / `robotctl enable` —— 发 RPC
//! - `robotctl tail <topic>` —— 通用 topic 打印
//! - `robotctl record` —— rosbag 式采集

#![forbid(unused_must_use)]

// 本 crate 暂只有 bin。空 lib.rs 让 `cargo test -p robot-cli` 能跑；未来 CLI 体积
// 增大后核心逻辑抽到 lib 里方便单测。
