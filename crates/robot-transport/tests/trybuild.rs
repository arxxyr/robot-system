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
//! TRYBUILD=overwrite ROBOT_TRYBUILD_CHILD=1 cargo test --test trybuild -p robot-transport
//! ```
//!
//! 覆盖 `tests/ui/*.stderr`，review diff 只看"是否仍是预期 error 种类"。
//! 若本次升级后 stderr 稳定下来（连续多次 nightly 都不再变），把更新的
//! `.stderr` 文件提交。CI 失败时先核验 snapshot 是否只是格式差异。

#[test]
fn compile_failures() {
    const CHILD: &str = "ROBOT_TRYBUILD_CHILD";
    const DONE: &str = "ROBOT_TRYBUILD_ALL_CASES_COMPLETED";
    if std::env::var_os(CHILD).as_deref() != Some(std::ffi::OsStr::new("1")) {
        // trybuild 1.0.121 将完整 workspace.dependencies 复制到临时单包 workspace，
        // Cargo 1.100 会为其未使用条目报清单警告。仅在隔离子进程解除汇总拒绝，
        // 父进程继续拒绝其它警告；正式 workspace 的编译及 Clippy 门禁不变。
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "compile_failures", "--nocapture"])
            .env(CHILD, "1")
            .env("CARGO_BUILD_WARNINGS", "warn")
            .env("CARGO_TERM_COLOR", "never")
            .env("TRYBUILD", "wip")
            .output()
            .expect("无法启动独立 trybuild 子进程");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stdout}\n{stderr}");
        assert!(
            stdout.lines().any(|line| line == DONE),
            "编译失败检查未全部完成：{stdout}\n{stderr}"
        );
        // mock 子工作区不使用 Rerun；只接受本仓库固定补丁的这一条未使用警告。
        let re_protos_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../3rd-party/re_protos")
            .canonicalize()
            .expect("无法定位仓库内的 re_protos 补丁");
        let unused_re_protos_patch = format!(
            "patch `re_protos v0.42.0 ({})` was not used in the crate graph",
            re_protos_path.display()
        );
        for line in stdout.lines().chain(stderr.lines()) {
            assert!(
                !line.starts_with("warning["),
                "trybuild 出现 Rust 警告：{line}\n{stdout}\n{stderr}"
            );
            if let Some(warning) = line.strip_prefix("warning:") {
                let warning = warning.trim_start();
                let generated_manifest_summary = warning
                    .strip_prefix("workspace (manifest) generated ")
                    .and_then(|text| text.strip_suffix(" warnings"))
                    .is_some_and(|count| count.parse::<usize>().is_ok());
                assert!(
                    warning.starts_with("unused workspace dependency `")
                        || generated_manifest_summary
                        || warning == unused_re_protos_patch,
                    "trybuild 出现非临时清单警告：{line}\n{stdout}\n{stderr}"
                );
            }
        }
        eprintln!("{stderr}");
        println!("{DONE}");
        return;
    }
    let t = trybuild::TestCases::new();
    #[cfg(feature = "mock")]
    {
        t.compile_fail("tests/ui/view_outlives_lease.rs");
        t.compile_fail("tests/ui/writer_use_after_publish.rs");
        t.compile_fail("tests/ui/second_loan_before_publish.rs");
        t.compile_fail("tests/ui/writer_use_after_publish_typed.rs");
    }
    // S48 A3b：snapshot 写端所有权与固定 Words 契约。
    t.compile_fail("tests/ui/snapshot_publisher_not_clone.rs");
    t.compile_fail("tests/ui/snapshot_publish_through_shared_ref.rs");
    t.compile_fail("tests/ui/snapshot_publish_through_arc.rs");
    t.compile_fail("tests/ui/snapshot_dynamic_words_rejected.rs");
    // trybuild 在析构时才真正运行，必须在成功完成之后发布标记。
    drop(t);
    println!("{DONE}");
}
