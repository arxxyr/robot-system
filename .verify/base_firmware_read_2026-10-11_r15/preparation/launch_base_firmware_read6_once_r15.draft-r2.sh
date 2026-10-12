#!/bin/bash
set -eu
umask 077
export PYTHONDONTWRITEBYTECODE=1
tmux set-option -p remain-on-exit on
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 /home/loosqk/robot-checks/position-return-20261010-r1/base-firmware-read-r15/candidate/base_firmware_read6_r15.py --config /home/loosqk/robot-checks/position-return-20261010-r1/base-firmware-read-r15/config.final.base_firmware_read6.r15.json --execute > /home/loosqk/robot-checks/position-return-20261010-r1/base-firmware-read-r15/read6-launch-record-r15/driver.log 2>&1
