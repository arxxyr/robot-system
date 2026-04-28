#!/usr/bin/env bash
# v0.3 A1-4 ClearEStop 端到端 smoke 入口（薄包装；reviewer 拍板形态 A）。
#
# 工作流：
#   1. cargo build --release 三个 binary：s0_daemon / robot_rpc_gateway /
#      clear_estop_e2e（feature `robot-tests/clear-estop-e2e`）。
#   2. 跑 `clear_estop_e2e --bin-dir target/release --case all` 顺序跑 5 个
#      case（selftest / reject_viewer / happy_dry_run / happy_real /
#      idem_replay），任一 FAIL 不中止后续。
#   3. **全脚本 stdout/stderr** 通过 `exec > >(tee ...)` 重定向，所有输出
#      （header / build / e2e / final summary）都落 `logs/clear_estop_smoke_<TS>.log`，
#      evidence 完整可复盘（reviewer A1-4 round 2 nit 修正：旧实现仅
#      build + e2e 输出 inline tee，header / final summary 不入 log）。
#
# 用途：A1-4 milestone evidence 复跑入口。CI / 人工 / Orin 真机都按本脚本
# 跑；handoff `docs/handoffs/v0.3_a1_4_clear_estop_smoke.md` 引用本脚本
# 路径作"可复跑"证据。
#
# 不在范围（reviewer A1-4 拍板）：
#   - reject_state / crash-replay 等 follow-up case；
#   - GitHub Actions / `.github/workflows/` CI 接入；
#   - `--allow-empty-latch` 撤销（A1-5 范围）。
#
# 退出码：透传 clear_estop_e2e 自身退出码（0 全过；1 CLI/preflight 错；
# 2 ≥ 1 case FAIL）；build 失败 → exit 1。

set -euo pipefail

# ── 切到 repo root（脚本可以从任意 cwd 调用）─────────────────────────────
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

LOG_DIR="$REPO_ROOT/logs"
TS="$(date -u +%Y%m%dT%H%M%SZ)"
LOG_PATH="$LOG_DIR/clear_estop_smoke_${TS}.log"
mkdir -p "$LOG_DIR"

# ── 全脚本输出重定向：stdout/stderr 都进 log + 仍在终端可见 ──────────────
# `exec > >(tee -a "$LOG_PATH") 2>&1` 把整个 shell 的 stdout/stderr 都管
# 到 tee 子进程，从此点起所有 echo / 子命令输出都同时落 log + 屏幕。
# 子命令退出码不被 tee 改变（不像 `cmd | tee` 形式 PIPESTATUS 才能拿原码），
# 直接 `$?` 读 e2e binary 的退出码即可。
exec > >(tee -a "$LOG_PATH") 2>&1

echo "==> repo root      : $REPO_ROOT"
echo "==> log file       : $LOG_PATH"
echo "==> hostname       : $(hostname)"
echo "==> uname          : $(uname -a)"
echo

# ── 1) release build s0_daemon / robot_rpc_gateway / clear_estop_e2e ─────
echo "==> cargo build --release（s0_daemon / robot_rpc_gateway / clear_estop_e2e）..."
cargo build --release \
    --bin s0_daemon \
    --bin robot_rpc_gateway \
    --bin clear_estop_e2e \
    --features "robot-tests/clear-estop-e2e"

# ── 2) 跑 --case all ─────────────────────────────────────────────────────
echo
echo "==> ./target/release/clear_estop_e2e --bin-dir target/release --case all"
echo

set +e
./target/release/clear_estop_e2e --bin-dir target/release --case all
SMOKE_EXIT=$?
set -e

# ── 3) summary ───────────────────────────────────────────────────────────
echo
echo "==================================================================="
echo "==> clear_estop_e2e exit code = $SMOKE_EXIT"
echo "==> full log                  = $LOG_PATH"
echo "==================================================================="

exit "$SMOKE_EXIT"
