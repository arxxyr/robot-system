#!/bin/bash
# 只由root在H2已退出且非UART清场通过后启动；STOP34未执行，不与RT测量并行。
set -u
umask 077
export PATH=/home/loosqk/.cargo/bin:/home/loosqk/.local/bin:$PATH
export PYTHONDONTWRITEBYTECODE=1
quality_base=/home/loosqk/robot-checks/h2-exit-observation-20261009-r1
tmux set-option -w -t "$TMUX_PANE" remain-on-exit on
uv run --no-project python "$quality_base/bootstrap/commit_quality_once_r2.py" > "$quality_base/bootstrap/commit-quality-r2-owner.log" 2>&1
quality_status=$?
printf '%s\n' "$quality_status" > "$quality_base/bootstrap/commit-quality-r2-owner.exit"
exit "$quality_status"
