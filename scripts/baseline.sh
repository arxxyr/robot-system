#!/usr/bin/env bash
#
# scripts/baseline.sh —— iceoryx2_rt_baseline 一键封装。
#
# 设计动机见 `docs/handoffs/2026-04-24_5.md` §5.4，与本次 CHANGELOG
# "2026-04-24 · Step 6.5 RT 调优搁置 + XLeRobot 不卡硬门槛" 同期落地。
#
# XLeRobot 不卡 `docs/00_design.md` §10.1 的 100 µs / 500 µs 硬门槛
# （Orin Nano 算力 + 非 PREEMPT_RT 内核做不到也不需要）。本脚本两种模式都
# 用 *宽阈值* —— harness 仍会因 sample_ratio < 90% 或服务启动失败 fail。
# 真要追门槛走 ATOM01 / 人形 target，到时候再加 strict 模式。
#
# 用法：
#   ./scripts/baseline.sh local                  # 本机 30s smoke（无 rtprio）
#   ./scripts/baseline.sh local 60s              # 本机 60s
#   ./scripts/baseline.sh orin                   # ssh Orin 跑 60s + cpu pin
#   ./scripts/baseline.sh orin 10m               # ssh Orin 跑 10min
#
# 退出码：透传 iceoryx2_rt_baseline（0 通过 / 1 fail / 2 启动错）。

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

MODE="${1:-local}"
DURATION="${2:-}"

# 宽阈值：CI 同款，仅作 harness 不退化 + sample_ratio 把关。
# 改严要走 docs/CHANGELOG.md（Orin 真门槛是 100 µs / 500 µs）。
P99_US="${BASELINE_P99_US:-50000}"   # 50 ms
P999_US="${BASELINE_P999_US:-100000}" # 100 ms

usage() {
    cat <<'EOF'
用法：
  ./scripts/baseline.sh local [DURATION]    本机 release smoke（默认 30s）
  ./scripts/baseline.sh orin  [DURATION]    ssh Orin 跑（默认 60s + --cpu 4）

环境变量：
  BASELINE_P99_US    p99 阈值（µs），默认 50000
  BASELINE_P999_US   p999 阈值（µs），默认 100000
  ORIN_HOST          Orin ssh 目标，默认 loosqk@10.0.1.41
  ORIN_REPO          Orin 上仓库路径，默认 ~/repo/robot/robot-system
EOF
}

case "$MODE" in
    local)
        DURATION="${DURATION:-30s}"
        cd "$REPO_ROOT"
        echo "[baseline] build release ..."
        cargo build -p robot-tests --release --bin iceoryx2_rt_baseline
        echo "[baseline] run local smoke duration=$DURATION p99≤${P99_US}µs p999≤${P999_US}µs"
        exec ./target/release/iceoryx2_rt_baseline \
            --duration "$DURATION" \
            --hz 50 \
            --p99-us "$P99_US" \
            --p999-us "$P999_US"
        ;;
    orin)
        DURATION="${DURATION:-60s}"
        ORIN_HOST="${ORIN_HOST:-loosqk@10.0.1.41}"
        ORIN_REPO="${ORIN_REPO:-~/repo/robot/robot-system}"
        echo "[baseline] ssh $ORIN_HOST → cargo build + run duration=$DURATION cpu=4 strict-rt"
        # shellcheck disable=SC2029  # 变量在远端 shell 展开是预期行为
        exec ssh "$ORIN_HOST" "
            source ~/.cargo/env && \
            cd $ORIN_REPO && \
            cargo build -p robot-tests --release --bin iceoryx2_rt_baseline && \
            ./target/release/iceoryx2_rt_baseline \
                --duration $DURATION \
                --hz 50 \
                --priority 70 \
                --cpu 4 \
                --strict-rt \
                --p99-us $P99_US \
                --p999-us $P999_US
        "
        ;;
    -h|--help|help)
        usage
        exit 0
        ;;
    *)
        echo "未知模式: $MODE" >&2
        usage >&2
        exit 2
        ;;
esac
