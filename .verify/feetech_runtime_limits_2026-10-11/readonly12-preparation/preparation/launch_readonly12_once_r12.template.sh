#!/bin/bash
set -eu
umask 077
export PYTHONDONTWRITEBYTECODE=1
tmux set-option -p remain-on-exit on
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 /home/loosqk/robot-checks/position-return-20261010-r1/runtime-limits-r12/readonly12_r12.py --config /home/loosqk/robot-checks/position-return-20261010-r1/runtime-limits-r12/config.readonly12.r12.json --execute > /home/loosqk/robot-checks/position-return-20261010-r1/runtime-limits-r12/readonly12-launch-record-r12/driver.log 2>&1
