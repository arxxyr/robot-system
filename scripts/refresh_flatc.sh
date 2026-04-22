#!/usr/bin/env bash
#
# refresh_flatc.sh —— 从 GitHub releases 拉 vendor flatc 二进制到
# 3rd-party/flatc/<platform>/flatc。
#
# 平台覆盖（与 docs/CHANGELOG.md 2026-04-22 "Step 2 工具链决策" 对齐）：
#   - darwin-arm64   ← Mac.flatc.binary.zip（arm64 原生）
#   - linux-x86_64   ← Linux.flatc.binary.g++-13.zip
#   - linux-aarch64  ← **无官方 release**；Orin 部署前由用户从源码编译，
#                      流程见 3rd-party/flatc/README.md
#
# 使用：
#   bash scripts/refresh_flatc.sh                   # 覆盖默认版本
#   bash scripts/refresh_flatc.sh 25.12.19          # 显式指定版本
#
# 退出码：
#   0  成功
#   1  工具缺失 / 网络失败 / 校验失败
#
# 本脚本不依赖 python / node，只依赖 POSIX: bash curl unzip awk +
# sha256sum (Linux) 或 shasum (macOS)。不考虑 Windows。

set -euo pipefail

# --------------------------------------------------------------------
# 配置
# --------------------------------------------------------------------

# 默认版本；允许命令行第一个参数覆盖
VERSION="${1:-25.12.19}"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VENDOR_DIR="$REPO_ROOT/3rd-party/flatc"
TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

RELEASE_BASE="https://github.com/google/flatbuffers/releases/download/v${VERSION}"

# 平台 → release asset 名映射
#   Mac.flatc.binary.zip 只含 arm64 原生二进制（≥ 24.x）；
#   Intel Mac 改用 MacIntel.flatc.binary.zip 并按需扩展。
PLATFORM_DARWIN_ARM64_ASSET="Mac.flatc.binary.zip"
PLATFORM_LINUX_X86_64_ASSET="Linux.flatc.binary.g++-13.zip"

# --------------------------------------------------------------------
# 工具兼容层（macOS / Linux）
# --------------------------------------------------------------------

sha256_file() {
    # 接受单个文件路径，输出 "<digest>  <filename>" 格式（两空格，GNU 兼容）
    local f="$1"
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$f"
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$f"
    else
        echo "error: neither sha256sum nor shasum is available" >&2
        return 1
    fi
}

require_tool() {
    local t="$1"
    command -v "$t" >/dev/null 2>&1 || {
        echo "error: required tool '$t' not found in PATH" >&2
        exit 1
    }
}

require_tool curl
require_tool unzip
require_tool awk

# --------------------------------------------------------------------
# 核心：fetch_one <platform> <asset_name>
# --------------------------------------------------------------------

fetch_one() {
    local platform="$1"
    local asset="$2"
    local url="${RELEASE_BASE}/${asset}"
    local zip_path="$TMPDIR/${platform}-${asset}"
    local target_dir="$VENDOR_DIR/$platform"

    echo "==> [${platform}] fetching ${asset}"
    curl --fail --silent --show-error --location --output "$zip_path" "$url"

    mkdir -p "$target_dir"
    unzip -o -q "$zip_path" -d "$target_dir"

    # 部分版本 zip 直接放 flatc；其他版本可能有子目录。规范化到 <target_dir>/flatc。
    if [[ ! -f "$target_dir/flatc" ]]; then
        local found
        found="$(find "$target_dir" -type f -name flatc -print -quit || true)"
        if [[ -n "$found" && "$found" != "$target_dir/flatc" ]]; then
            mv "$found" "$target_dir/flatc"
        fi
    fi

    if [[ ! -f "$target_dir/flatc" ]]; then
        echo "error: flatc binary missing after unzip for ${platform}" >&2
        return 1
    fi

    chmod +x "$target_dir/flatc"

    # 版本自检。仅在 host == platform 时才能真正执行；其他情况仅报告。
    if [[ "$platform" == "$(detect_host_platform)" ]]; then
        local actual
        actual="$("$target_dir/flatc" --version 2>/dev/null || true)"
        echo "    host self-check: $actual"
        case "$actual" in
            *"$VERSION"*) : ;;
            *)
                echo "warning: host self-check version mismatch (expected $VERSION, got '$actual')" >&2
                ;;
        esac
    else
        echo "    (cross-platform, no self-check)"
    fi
}

# --------------------------------------------------------------------
# Host 平台探测（与 build.rs 保持相同判定）
# --------------------------------------------------------------------

detect_host_platform() {
    local os arch
    os="$(uname -s)"
    arch="$(uname -m)"
    case "$os/$arch" in
        Darwin/arm64)  echo "darwin-arm64"   ;;
        Darwin/x86_64) echo "darwin-x86_64"  ;;   # 未 vendor，仅报告
        Linux/x86_64)  echo "linux-x86_64"   ;;
        Linux/aarch64) echo "linux-aarch64"  ;;
        Linux/arm64)   echo "linux-aarch64"  ;;
        *) echo "unsupported ($os/$arch)" ;;
    esac
}

# --------------------------------------------------------------------
# 主流程
# --------------------------------------------------------------------

echo "==> refresh_flatc.sh: target flatc version v${VERSION}"
echo "==> vendor dir: $VENDOR_DIR"
echo "==> host platform: $(detect_host_platform)"

mkdir -p "$VENDOR_DIR"
echo "$VERSION" > "$VENDOR_DIR/VERSION"

fetch_one "darwin-arm64" "$PLATFORM_DARWIN_ARM64_ASSET"
fetch_one "linux-x86_64" "$PLATFORM_LINUX_X86_64_ASSET"

# linux-aarch64 目录保留给 Orin 部署，README 说明手动编译流程。
mkdir -p "$VENDOR_DIR/linux-aarch64"

# --------------------------------------------------------------------
# 写 checksums.sha256
#   文件内容：<digest>  <platform>/flatc      # 每行一份
#   签名覆盖所有实际存在的二进制（linux-aarch64 没有 binary 时跳过）
# --------------------------------------------------------------------

CHECKSUMS_TMP="$TMPDIR/checksums.sha256"
: > "$CHECKSUMS_TMP"
for platform in darwin-arm64 linux-x86_64 linux-aarch64; do
    local_path="$VENDOR_DIR/$platform/flatc"
    if [[ -f "$local_path" ]]; then
        digest="$(sha256_file "$local_path" | awk '{print $1}')"
        printf '%s  %s/flatc\n' "$digest" "$platform" >> "$CHECKSUMS_TMP"
    else
        printf '# %s/flatc  MISSING (build from source, see README.md)\n' "$platform" >> "$CHECKSUMS_TMP"
    fi
done
mv "$CHECKSUMS_TMP" "$VENDOR_DIR/checksums.sha256"

echo
echo "==> done. review diff and git commit:"
echo "      git add 3rd-party/flatc scripts/refresh_flatc.sh"
echo "      git diff --cached --stat 3rd-party/flatc"
