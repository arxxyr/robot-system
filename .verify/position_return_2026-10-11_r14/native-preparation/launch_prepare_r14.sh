#!/bin/bash
set -eu
umask 077
export PYTHONDONTWRITEBYTECODE=1
tmux set-option -p remain-on-exit on
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 /home/loosqk/robot-checks/position-return-20261010-r1/head2-p32-trial-r14-prepare-boot/prepare_r14.py > /home/loosqk/robot-checks/position-return-20261010-r1/head2-p32-trial-r14-prepare-boot/driver.log 2>&1
