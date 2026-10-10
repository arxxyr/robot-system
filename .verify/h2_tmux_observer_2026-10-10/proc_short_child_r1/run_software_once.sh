#!/bin/bash
# 仅运行本次无害软件实验，不运行 H2、服务或 UART。
set -euC
export PATH=/home/loosqk/.local/bin:/home/loosqk/.cargo/bin:$PATH
export PYTHONDONTWRITEBYTECODE=1
task_base=/home/loosqk/robot-checks/proc-reader-short-child-20261010-r1
tmux set-option -w -t "$TMUX_PANE" remain-on-exit on
exec > "$task_base/owner.log" 2>&1
task_python=$(uv python find 3.12)
printf 'software_only_owner_pid=%s\npython=%s\n' "$$" "$task_python"
exec "$task_python" "$task_base/experiment_once.py"
