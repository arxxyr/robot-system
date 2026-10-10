#!/bin/bash
# 本次纯软件提交质量门，tmux保留真实owner退出码。
set -u
umask 077
export PATH=/home/loosqk/.cargo/bin:/home/loosqk/.local/bin:$PATH
export PYTHONDONTWRITEBYTECODE=1
quality_base=/home/loosqk/robot-checks/h2-right-r2-commit-quality-20261010-r1
tmux set-option -p -t "$TMUX_PANE" remain-on-exit on
uv run --no-project python "$quality_base/bootstrap/commit_quality_once_r1.py" > "$quality_base/bootstrap/quality-owner.log" 2>&1
quality_status=$?
printf '%s\n' "$quality_status" > "$quality_base/bootstrap/quality-owner.exit"
exit "$quality_status"
