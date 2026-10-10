#!/bin/bash
# 只运行一次，tmux 保留 owner 真实退出码。
set -u
umask 077
export PATH=/home/loosqk/.cargo/bin:/home/loosqk/.local/bin:$PATH
export PYTHONDONTWRITEBYTECODE=1
software_base=/home/loosqk/robot-checks/h2-exit-observation-20261009-r1
tmux set-option -w -t "$TMUX_PANE" remain-on-exit on
uv run --no-project python "$software_base/bootstrap/control-v3/source_sync_894_once.py" > "$software_base/bootstrap/source-sync-r2-owner.log" 2>&1
software_status=$?
printf '%s\n' "$software_status" > "$software_base/bootstrap/source-sync-r2-owner.exit"
exit "$software_status"
