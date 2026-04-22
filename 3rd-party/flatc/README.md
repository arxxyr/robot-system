# vendor flatc

> FlatBuffers schema 编译器 `flatc` 的 vendor 副本。
> Version pin：与 `flatbuffers` Rust crate 一一对应，当前 `VERSION` 文件为权威来源。
> 存在理由：`robot-msg` 的 `build.rs` 绝不依赖系统 `flatc`，保证 "无网可 build" 与 "clean host 可 build"（参见 `docs/01_implementation_steps.md` Step 2 退出标准）。

## 目录结构

```
3rd-party/flatc/
├─ README.md                     # 本文件
├─ VERSION                       # flatc 版本号（与 Cargo.toml workspace.dependencies.flatbuffers 对齐）
├─ checksums.sha256              # 每份二进制的 sha256 摘要
├─ darwin-arm64/flatc            # macOS Apple Silicon (Mac.flatc.binary.zip)
├─ linux-x86_64/flatc            # Linux x86_64 (Linux.flatc.binary.g++-13.zip)
└─ linux-aarch64/                # Orin 等 Linux arm64；官方 release 无二进制，见下方"从源码编译"
```

## 平台覆盖

| 平台 | 来源 | 状态 |
|------|------|------|
| `darwin-arm64` | GitHub release `Mac.flatc.binary.zip` | ✅ vendored |
| `linux-x86_64` | GitHub release `Linux.flatc.binary.g++-13.zip` | ✅ vendored |
| `linux-aarch64` | **官方 release 无 arm64 二进制**，从源码编译 | ⚠ 空目录；Orin 部署前填充 |

Windows 不支持：项目仅把 Windows 作为 S2 客户端，`robot-msg` 的 `build.rs` 不在 Windows 上跑。

## 升级流程

1. 确认目标 flatc 版本（需与 `Cargo.toml` 的 `flatbuffers = "x.y.z"` 一致）。
2. 跑刷新脚本：

   ```bash
   bash scripts/refresh_flatc.sh 25.12.19    # 指定版本号
   # 或用当前 VERSION 文件里的值：
   bash scripts/refresh_flatc.sh
   ```

3. 审阅 diff：`VERSION` / `checksums.sha256` / 各平台 `flatc` 二进制大小。
4. `git add 3rd-party/flatc` 并 commit，message 模板：

   ```
   ⬆️ build: 升级 vendor flatc 到 vX.Y.Z

   - 同步升级 workspace.dependencies.flatbuffers 到 "X.Y.Z"
   - checksums.sha256 更新
   ```

5. 本地 `cargo build -p robot-msg` 跑一轮，确认新版本生成代码编译通过；`cargo run -p robot-msg --bin schema_lint` 保证静态校验仍过。

## Linux aarch64（Orin）从源码编译

Google 上游目前不分发 `flatc` 的 Linux arm64 预编译二进制。在 Orin 首次部署前按下列流程填充 `linux-aarch64/flatc`：

### 在 Orin 上直接编译

```bash
VERSION=$(cat 3rd-party/flatc/VERSION)
git clone --depth 1 --branch v${VERSION} https://github.com/google/flatbuffers.git /tmp/flatbuffers-${VERSION}
cd /tmp/flatbuffers-${VERSION}
cmake -S . -B build -DCMAKE_BUILD_TYPE=Release -DFLATBUFFERS_BUILD_TESTS=OFF
cmake --build build --target flatc -j"$(nproc)"

# 放入 vendor
cp build/flatc <robot-system-repo>/3rd-party/flatc/linux-aarch64/flatc
chmod +x <robot-system-repo>/3rd-party/flatc/linux-aarch64/flatc

# 更新 checksums.sha256：把新行加入已有文件
pushd <robot-system-repo>/3rd-party/flatc
digest=$(sha256sum linux-aarch64/flatc | awk '{print $1}')
# 用新摘要替换原来的 "MISSING" 行
sed -i.bak -E "s|^# linux-aarch64/flatc.*\$|${digest}  linux-aarch64/flatc|" checksums.sha256
rm checksums.sha256.bak
popd
```

### 在 x86_64 开发机上交叉编译（可选）

需要 `aarch64-linux-gnu-gcc` 工具链。实际运维反馈 Orin 上直接编译（5–10 分钟）比搭交叉工具链更省事，这里不展开。

### 校验

填充后跑：

```bash
sha256sum -c 3rd-party/flatc/checksums.sha256
./3rd-party/flatc/linux-aarch64/flatc --version    # 必须输出 "flatc version <VERSION>"
```

## sha256 校验

任何人拿到仓库后可跑一次：

```bash
cd 3rd-party/flatc
sha256sum -c checksums.sha256 2>&1 | grep -v MISSING
```

期望全部 `OK`（`MISSING` 行对应尚未填充的 linux-aarch64，属正常）。
