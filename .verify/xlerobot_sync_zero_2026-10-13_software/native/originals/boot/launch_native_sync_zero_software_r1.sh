#!/bin/bash
set -eu
export PYTHONDONTWRITEBYTECODE=1
tmux set-option -p remain-on-exit on
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 /home/loosqk/robot-checks/xlerobot-sync-zero-software-20261013/boot/native_sync_zero_software_r1.py > /home/loosqk/robot-checks/xlerobot-sync-zero-software-20261013/boot/xlerobot-sync-zero-software-driver.log 2>&1
