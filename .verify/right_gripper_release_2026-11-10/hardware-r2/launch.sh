#!/usr/bin/env bash
set -u
run_root=/home/loosqk/robot-checks/right-gripper-release-20261110-r2
printf "%s\n" "$$" > "$run_root/launch/pid"
date -u +%Y-%m-%dT%H:%M:%S.%NZ > "$run_root/launch/started-at"
export PYTHONDONTWRITEBYTECODE=1
/home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 -I -S -B "$run_root/run_hardware_maintenance.py" --bus-config "$run_root/xlerobot_bus2_full.toml" --bindings "$run_root/bindings.json" --output "$run_root/capture" --run-id right6-release-fixed-start --artifact-root "$run_root" --right-gripper-release --execute --approval "$run_root/go.json" > "$run_root/launch/stdout.log" 2> "$run_root/launch/stderr.log"
result=$?
printf "%s\n" "$result" > "$run_root/launch/exit-code"
exit "$result"
