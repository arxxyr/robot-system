#!/bin/bash
# 本次固定软件批次；信号不稳定时由既有tmux保留进程。
set -u
umask 077
export PATH=/home/loosqk/.cargo/bin:/home/loosqk/.local/bin:$PATH
export PYTHONDONTWRITEBYTECODE=1
release_base=/home/loosqk/robot-checks/h2-exit-observation-20261009-r1
tmux set-option -w -t "$TMUX_PANE" remain-on-exit on
uv run --no-project python "$release_base/bootstrap/prepare_release_once.py" > "$release_base/bootstrap/release-r1-owner.log" 2>&1
release_status=$?
printf '%s\n' "$release_status" > "$release_base/bootstrap/release-r1-owner.exit"
exit "$release_status"
