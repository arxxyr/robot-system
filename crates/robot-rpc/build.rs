// build.rs 是工程/工具代码，不走核心 crate 的 pedantic 严格级。
#![allow(clippy::pedantic, clippy::nursery)]

//! robot-rpc · build.rs
//!
//! ### 工作流
//!
//! 1. 用 `protoc-bin-vendored` 提供的 vendored `protoc` binary（避免 host
//!    必须装 protoc）。
//! 2. 调 `tonic-build::configure().compile_protos(...)` 把
//!    `proto/robot_control.proto` 生成 Rust 代码到 `$OUT_DIR`。
//! 3. lib.rs 通过 `tonic::include_proto!("robot.control.v1")` 引入。
//!
//! 跟 robot-msg/build.rs（vendor flatc）同款 vendor 隔离策略：编译期一致
//! 性 + 跨开发机不依赖系统包。

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. vendored protoc
    let protoc = protoc_bin_vendored::protoc_bin_path()
        .map_err(|e| format!("protoc-bin-vendored: {e:?}"))?;
    // SAFETY: build.rs 单线程跑；env_var 只在本进程内生效，不影响其它 crate。
    unsafe {
        std::env::set_var("PROTOC", &protoc);
    }

    // 2. tonic-build 生成。**A1-3 (3/3-a)** 拍板：默认仅 server stub；开
    //    `client` feature（cargo 自动 set `CARGO_FEATURE_CLIENT=1`）才生成
    //    client stub，给 `robot-tests/src/bin/clear_estop_e2e.rs` 端到端
    //    smoke 用。生产二进制构建不带 `client`，无 client-only runtime 影响。
    let build_client = std::env::var_os("CARGO_FEATURE_CLIENT").is_some();
    tonic_build::configure()
        .build_server(true)
        .build_client(build_client)
        .compile_protos(&["proto/robot_control.proto"], &["proto"])?;

    // 3. trigger rerun
    println!("cargo:rerun-if-changed=proto/robot_control.proto");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_CLIENT");
    Ok(())
}
