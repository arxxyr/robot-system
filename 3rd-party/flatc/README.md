# 仓库内的 flatc

[robot-msg 构建脚本](../../crates/robot-msg/build.rs)按**构建主机**选择 FlatBuffers 编译器。仓库已包含三个平台的可执行文件，版本由 [VERSION](VERSION) 和根工作区 `flatbuffers` 依赖共同固定。

| 构建主机 | 仓库路径 | 来源 |
|---|---|---|
| macOS arm64 | `darwin-arm64/flatc` | 上游发布包 `Mac.flatc.binary.zip` |
| Linux x86_64 | `linux-x86_64/flatc` | 上游发布包 `Linux.flatc.binary.g++-13.zip` |
| Linux aarch64（Orin） | `linux-aarch64/flatc` | 在 Orin 从对应上游标签源码编译 |

## 校验

在本目录执行。清单含三个平台的 SHA256，所有行均应为 `OK`。

```sh
shasum -a 256 -c checksums.sha256
```

Linux 也可用 `sha256sum -c checksums.sha256`。在 Orin 上可另用 `./linux-aarch64/flatc --version` 核对本机程序与 `VERSION`。

## 升级

1. 在根 `Cargo.toml` 确定目标 `flatbuffers` 版本。
2. 在 Orin 从同一版本的上游标签编译并替换本平台程序。以下命令在项目根执行，将示例版本改为实际目标版本。

   ```sh
   flatc_version=25.12.19
   flatc_tmp=$(mktemp -d)
   git clone --depth 1 --branch "v${flatc_version}" https://github.com/google/flatbuffers.git "${flatc_tmp}/src"
   cmake -S "${flatc_tmp}/src" -B "${flatc_tmp}/build" -DCMAKE_BUILD_TYPE=Release -DFLATBUFFERS_BUILD_TESTS=OFF
   cmake --build "${flatc_tmp}/build" --target flatc --parallel
   "${flatc_tmp}/build/flatc" --version
   install -m 755 "${flatc_tmp}/build/flatc" 3rd-party/flatc/linux-aarch64/flatc
   ```

3. 将上述 aarch64 产物放在执行[刷新脚本](../../scripts/refresh_flatc.sh)的工作树内，再执行 `bash scripts/refresh_flatc.sh "$flatc_version"`。脚本下载 macOS arm64 和 Linux x86_64 两份程序，并基于三份现有文件重写 `VERSION` 和摘要。
4. 核对三行摘要及构建主机的版本。使用隔离构建目录执行 `cargo run --locked -p robot-msg --bin schema_lint`，验证消息生成与静态检查。
