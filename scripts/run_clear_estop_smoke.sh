#!/usr/bin/env bash
# ClearEStop 五场景：selftest / reject_viewer / happy_dry_run / happy_real / idem_replay。
# 全部输出写入 logs/clear_estop_smoke_<UTC>.log；透传构建或 harness 的真实退出码。
# harness 使用隔离 mock 配置及自行准备的 latch，不访问现场串口。

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

# 外部 target 目录只作父目录，smoke 不覆盖其 release 中的正式产物。
SMOKE_TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}/clear-estop-smoke"
mkdir -p "$SMOKE_TARGET_DIR"
SMOKE_TARGET_DIR="$(cd "$SMOKE_TARGET_DIR" && pwd)"

LOG_DIR="$REPO_ROOT/logs"
TS="$(date -u +%Y%m%dT%H%M%SZ)"
LOG_PATH="$LOG_DIR/clear_estop_smoke_${TS}.log"
mkdir -p "$LOG_DIR"

exec > >(tee -a "$LOG_PATH") 2>&1

# 本脚本在本机执行产物，显式 host 避免外部交叉目标改变输出目录或架构。
RUSTC_VERBOSE="$("${RUSTC:-rustc}" -vV)"
HOST_TARGET="$(printf '%s\n' "$RUSTC_VERBOSE" | sed -n 's/^host: //p')"
if [[ -z "$HOST_TARGET" ]]; then
    echo "无法读取 rustc host 目标" >&2
    exit 1
fi
BIN_DIR="$SMOKE_TARGET_DIR/$HOST_TARGET/release"

echo "==> repo root      : $REPO_ROOT"
echo "==> log file       : $LOG_PATH"
echo "==> binary dir     : $BIN_DIR"
echo "==> hostname       : $(hostname)"
echo "==> uname          : $(uname -a)"
echo

echo "==> cargo build --release（s0_daemon / robot_rpc_gateway / clear_estop_e2e）..."
cargo build --locked --release \
    --target-dir "$SMOKE_TARGET_DIR" \
    --target "$HOST_TARGET" \
    --bin s0_daemon \
    --bin robot_rpc_gateway \
    --bin clear_estop_e2e \
    --features "robot-tests/clear-estop-e2e"

echo
echo "==> $BIN_DIR/clear_estop_e2e --bin-dir $BIN_DIR --case all"
echo

set +e
"$BIN_DIR/clear_estop_e2e" --bin-dir "$BIN_DIR" --case all
SMOKE_EXIT=$?
set -e

echo
echo "==================================================================="
echo "==> clear_estop_e2e exit code = $SMOKE_EXIT"
echo "==> full log                  = $LOG_PATH"
echo "==================================================================="

exit "$SMOKE_EXIT"
