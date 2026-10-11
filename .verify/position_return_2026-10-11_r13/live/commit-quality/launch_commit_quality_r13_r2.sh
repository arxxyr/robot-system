#!/bin/bash
set -eu
umask 077
export PYTHONDONTWRITEBYTECODE=1
tmux set-option -p remain-on-exit on
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 /home/loosqk/robot-checks/position-return-20261011-r13/head1-p32-commit-quality/boot/commit_quality_once_r13.py > /home/loosqk/robot-checks/position-return-20261011-r13/head1-p32-commit-quality/boot/driver.log 2>&1
