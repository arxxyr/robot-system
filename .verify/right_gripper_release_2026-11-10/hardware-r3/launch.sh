#!/usr/bin/env bash
set -u
run_root=/home/loosqk/robot-checks/right-gripper-release-20261110-r3
printf "%s\n" "$$" > "$run_root/launch/pid"
date -u +%Y-%m-%dT%H:%M:%S.%NZ > "$run_root/launch/started-at"
export PYTHONDONTWRITEBYTECODE=1
/home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 -I -S -B /home/loosqk/robot-checks/right-gripper-release-20261110-r3/run_hardware_maintenance.py --bus-config /home/loosqk/robot-checks/right-gripper-release-20261110-r3/xlerobot_bus2_full.toml --bindings /home/loosqk/robot-checks/right-gripper-release-20261110-r3/bindings.json --output /home/loosqk/robot-checks/right-gripper-release-20261110-r3/capture --run-id right6-release-1600 --artifact-root /home/loosqk/robot-checks/right-gripper-release-20261110-r3 --right-gripper-release --execute --approval /home/loosqk/robot-checks/right-gripper-release-20261110-r3/go.json > "$run_root/launch/stdout.log" 2> "$run_root/launch/stderr.log"
result=$?
printf "%s\n" "$result" > "$run_root/launch/exit-code"
exit "$result"
