#!/bin/bash
set -eu
export PYTHONDONTWRITEBYTECODE=1
tmux set-option -p remain-on-exit on
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 /home/loosqk/robot-checks/base-zero-sync-r1-commit-quality-20261013/boot/commit_quality_once_base_zero_sync_r1.py > /home/loosqk/robot-checks/base-zero-sync-r1-commit-quality-20261013/boot/base-zero-sync-r1-commit-quality-driver.log 2>&1
