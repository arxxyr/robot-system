#!/bin/bash
# 本轮仅nightly/fmt/根Clippy，既有Runner真实wait；无UART/服务/安装。
set -uC
umask 077
export PATH=/home/loosqk/.cargo/bin:/home/loosqk/.local/bin:$PATH
export PYTHONDONTWRITEBYTECODE=1
quality_base=/home/loosqk/robot-checks/h2-exit-observation-20261009-r1
tmux set-option -w -t "$TMUX_PANE" remain-on-exit on
uv run --no-project python "$quality_base/bootstrap/position-quality-r1.py" > "$quality_base/bootstrap/position-quality-r1-owner.log" 2>&1
quality_status=$?
printf '%s\n' "$quality_status" > "$quality_base/bootstrap/position-quality-r1-owner.exit"
exit "$quality_status"
