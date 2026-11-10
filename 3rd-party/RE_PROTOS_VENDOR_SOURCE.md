# vendor re_protos 0.42.0 来源与最小构建补丁

本目录用于解除 `re_protos 0.42.0` 与本项目最新 `protoc-bin-vendored 3.3.0` 的精确版本冲突。不改运行时代码、生成 Rust 或 protobuf 协议；根 workspace 的 patch/exclude 与锁文件由根依赖图管理。

## 原件身份

- 发布包：[re_protos-0.42.0.crate](https://static.crates.io/crates/re_protos/re_protos-0.42.0.crate)。
- 原包大小：231060 字节；SHA256：`c1ba8df9caee6c880ebcd490e9ab807695f8592922fc1666e06806dffc5ecde2`，与接入前根锁文件的 registry checksum 一致。
- `.cargo_vcs_info.json` 的上游提交：`30d87ed848c35425056a1b8668639850ae6786f0`；仓库内路径：`crates/store/re_protos`。
- 上游源码：[该提交的 re_protos](https://github.com/rerun-io/rerun/tree/30d87ed848c35425056a1b8668639850ae6786f0/crates/store/re_protos)。
- 原包含 33 个普通文件，已拒绝绝对路径、父目录穿越、重复路径、链接及特殊文件后解包。仓库不额外保存 `.crate` 或另一份源码副本。

## 唯一构建改动

实际 `Cargo.toml` 的 `[build-dependencies.protoc-bin-vendored]`：`version = "=3.2.0"` 改为 `version = "=3.3.0"`。其他 manifest 内容保持；原包含 `README.md`，因此保留原 readme 声明。`Cargo.toml.orig` 与包内 `Cargo.lock` 保持原件，后者不作为根 workspace 的解析入口。

上游 `build.rs` 仅用 vendored protoc 和其 include 路径生成带 imports/source info 的 `FileDescriptorSet`，不重生成已发布 Rust 类型。本地只调整该构建工具的依赖版本，不修改构建脚本或它的参数。

接入时逐文件 SHA256 核对：除实际 `Cargo.toml` 外，原包其余 32 个文件完全一致，其中 23 个 `.rs`（含构建脚本、测试和生成类型）及 5 个 `.proto` 均未变化。编译后的 descriptor 兼容性须由本项目定向测试验证，文件相同不代替编译与功能验收。

## 补齐上游许可原件

原发布包的 manifest 声明 `MIT OR Apache-2.0`，但包内未附许可文件。以下两件从同一上游提交的仓库根取得，原文补入本目录，不改许可内容：

| 文件 | 字节数 | SHA256 |
|---|---:|---|
| `LICENSE-APACHE` | 10850 | `8173d5c29b4f956d532781d2b86e4e30f83e6b7878dce18c919451d6ba707c90` |
| `LICENSE-MIT` | 1087 | `7d3db59468f92d70f176170deacf8ac6858b6d78010a30ae01eba42bac302940` |

来源分别为 [LICENSE-APACHE](https://github.com/rerun-io/rerun/blob/30d87ed848c35425056a1b8668639850ae6786f0/LICENSE-APACHE) 与 [LICENSE-MIT](https://github.com/rerun-io/rerun/blob/30d87ed848c35425056a1b8668639850ae6786f0/LICENSE-MIT)。

## 移除条件

上游发布版本允许与本项目统一的最新 `protoc-bin-vendored` 共存，并通过同源 descriptor 解码、Rerun 输出和相关质量门后，移除根 patch/exclude 及本地 vendor。此补丁不作为长期固定旧 protobuf 工具链或扩大协议修改的依据。
