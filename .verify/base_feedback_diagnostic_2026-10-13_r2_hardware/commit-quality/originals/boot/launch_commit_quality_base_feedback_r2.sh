#!/bin/bash
set -eu
export PYTHONDONTWRITEBYTECODE=1
tmux set-option -p remain-on-exit on
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 /home/loosqk/robot-checks/base-feedback-r2-commit-quality-20261013/boot/commit_quality_once_base_feedback_r2.py > /home/loosqk/robot-checks/base-feedback-r2-commit-quality-20261013/boot/base-feedback-r2-commit-quality-20261013-driver.log 2>&1
