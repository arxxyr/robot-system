//! `tests/common/` shared harness for `tests/task_rpc_e2e.rs`（B4 (3/3)）。
//!
//! cargo `tests/common/mod.rs` 是 cargo 识别的"非独立 test binary"模式：
//! cargo test 不把 `common/` 内文件当独立 test，仅作 `task_rpc_e2e.rs`
//! 引用的 module 编译。
//!
//! ## 子 module 依赖图
//!
//! ```text
//! task_rpc_e2e.rs (#[tokio::test] × 6)
//!         │
//!         └── common::case_ctx
//!                   ├── common::fake_state    (独立 std::thread + iceoryx2 publisher)
//!                   ├── common::subprocess    (tokio::process::Child + drain task)
//!                   ├── common::ready         (3 段式 marker / TCP / RPC probe)
//!                   └── common::client        (proto cmd builders + 4 token const)
//! ```
//!
//! 各 sub-module 边界详各自顶部 doc。

#![allow(dead_code)] // 测试侧 helper 可能仅部分 test fn 用到；不每条都 dead_code allow 太碎
#![allow(unreachable_pub)] // tests/common 是 cargo test 的 sub-mod，pub 不会对外暴露；workspace lint = warn
#![allow(clippy::doc_markdown)]
#![allow(clippy::missing_errors_doc)]

pub mod case_ctx;
pub mod client;
pub mod fake_heartbeat;
pub mod fake_state;
pub mod ready;
pub mod subprocess;
