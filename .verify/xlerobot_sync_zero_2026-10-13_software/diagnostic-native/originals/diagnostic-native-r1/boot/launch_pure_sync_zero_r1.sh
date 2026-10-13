#!/bin/bash
set -eu
export PYTHONDONTWRITEBYTECODE=1
tmux set-option -p remain-on-exit on
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 /home/loosqk/robot-checks/xlerobot-sync-zero-software-20261013/diagnostic-native-r1/boot/verify_pure_sync_zero_r1.py > /home/loosqk/robot-checks/xlerobot-sync-zero-software-20261013/diagnostic-native-r1/boot/sync-zero-diagnostic-software-driver.log 2>&1
