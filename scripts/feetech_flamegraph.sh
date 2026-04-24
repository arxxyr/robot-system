#!/usr/bin/env bash
#
# scripts/feetech_flamegraph.sh —— Step 7 task 27c 火焰图配套。
#
# 跑 feetech bench 中最热的 case（默认 `bus_sync/sync_read_present_position_13motor`）
# 长循环 + perf record + flamegraph SVG 输出。task 28 上 Orin 真机前必须看一眼，
# 确认 protocol / encoding / bus 没有意外热点。
#
# ## 前置（一次性）
#
#   cargo install flamegraph
#   sudo apt install -y linux-tools-common linux-tools-generic   # perf
#   echo -1 | sudo tee /proc/sys/kernel/perf_event_paranoid       # 允许 perf record
#
# ## 用法
#
#   ./scripts/feetech_flamegraph.sh                                # 默认 case
#   ./scripts/feetech_flamegraph.sh sync_write_goal_position_13motor
#   ./scripts/feetech_flamegraph.sh build_sync_write_13motor_2byte
#
# 输出：`target/criterion-flamegraph/<case>.svg`
#
# ## 退出码
#
# - 0：SVG 生成成功
# - 1：cargo flamegraph 不存在（提示安装）
# - 2：参数 / 环境异常

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

CASE="${1:-sync_read_present_position_13motor}"
DURATION_S="${FLAMEGRAPH_DURATION_S:-30}"  # bench 跑多久（criterion --measurement-time）

OUT_DIR="$REPO_ROOT/target/criterion-flamegraph"
OUT_SVG="$OUT_DIR/${CASE}.svg"

# ── 工具检查 ─────────────────────────────────────────────────────────────────
if ! command -v cargo-flamegraph >/dev/null 2>&1 \
   && ! cargo flamegraph --help >/dev/null 2>&1; then
    cat <<'EOF' >&2
错误：cargo-flamegraph 未安装。

安装：
  cargo install flamegraph

并确保 perf 可用（需 root 调权）：
  sudo apt install -y linux-tools-common linux-tools-generic
  echo -1 | sudo tee /proc/sys/kernel/perf_event_paranoid
EOF
    exit 1
fi

if ! command -v perf >/dev/null 2>&1; then
    echo "错误：perf 不在 PATH。安装 linux-tools-generic 后再试。" >&2
    exit 1
fi

paranoid="$(cat /proc/sys/kernel/perf_event_paranoid 2>/dev/null || echo "?")"
if [ "$paranoid" != "-1" ] && [ "$paranoid" != "0" ]; then
    cat <<EOF >&2
警告：/proc/sys/kernel/perf_event_paranoid = $paranoid（需要 ≤ 0 才能 record kernel/user 栈）。
临时放开：
  echo -1 | sudo tee /proc/sys/kernel/perf_event_paranoid
继续尝试（可能采样不全）...
EOF
fi

mkdir -p "$OUT_DIR"
cd "$REPO_ROOT"

echo "[flamegraph] case=$CASE  duration=${DURATION_S}s  out=$OUT_SVG"

# ── 跑 cargo flamegraph ─────────────────────────────────────────────────────
#
# `cargo flamegraph --bench feetech_bench` 用 release profile 跑 bench harness，
# perf record 全程采样，结束后用 inferno 生成 SVG。
#
# `--`  之后的参数透传给 criterion：只跑指定 case，并拉长 measurement-time
# 让 perf 采到足够样本（默认 5s 太短）。
cargo flamegraph \
    -p robot-adapter-feetech \
    --bench feetech_bench \
    --output "$OUT_SVG" \
    -- \
    "$CASE" \
    --warm-up-time 1 \
    --measurement-time "$DURATION_S"

echo "[flamegraph] 完成: $OUT_SVG"
echo "  浏览：firefox $OUT_SVG"
