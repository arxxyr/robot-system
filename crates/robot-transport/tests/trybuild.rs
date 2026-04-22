//! trybuild 负向测试入口。
//!
//! 每个 `tests/ui/*.rs` 都应该**编译失败**。trybuild 会调 cargo check 验证。
//!
//! ### nightly + stderr snapshot 的脆弱性
//!
//! Rust nightly 的 rustc 诊断格式几乎每次 release 都会调（span / label 顺序 /
//! 颜色 escape / note 文案）。一旦挂，跑：
//!
//! ```bash
//! TRYBUILD=overwrite cargo test --test trybuild -p robot-transport
//! ```
//!
//! 覆盖 `tests/ui/*.stderr`，review diff 只看"是否仍是预期 error 种类"。
//! 若本次升级后 stderr 稳定下来（连续多次 nightly 都不再变），把更新的
//! `.stderr` 文件提交。CI 失败时先核验 snapshot 是否只是格式差异。

#[test]
fn compile_failures() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/view_outlives_lease.rs");
    t.compile_fail("tests/ui/writer_use_after_publish.rs");
    t.compile_fail("tests/ui/second_loan_before_publish.rs");
}
