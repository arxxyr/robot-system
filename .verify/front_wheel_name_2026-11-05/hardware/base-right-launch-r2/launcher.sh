#!/usr/bin/env bash
set -u
task_root=/home/loosqk/robot-checks/front-wheel-name-20261105
printf '%s\n' "$$" > "$task_root/base-right-launch-r2/pid"
date -u +%Y-%m-%dT%H:%M:%S.%NZ > "$task_root/base-right-launch-r2/started-at"
export PATH=/home/loosqk/.local/bin:/home/loosqk/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
export PYTHONDONTWRITEBYTECODE=1
/home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 -I -S /home/loosqk/robot-checks/s0-bus2-raw-maintenance-20261023-r1/software/scripts/run_hardware_maintenance.py \
  --bindings "$task_root/mixed-bindings.json" \
  --output "$task_root/base-right-hardware-r2" \
  --run-id base-right-zero-observation-r2 \
  --artifact-root "$task_root" \
  --right-wheel-pulse --zero-stop-check --execute --approval "$task_root/base-right-go-r2.json" \
  > "$task_root/base-right-launch-r2/stdout.log" 2> "$task_root/base-right-launch-r2/stderr.log"
result=$?
printf '%s\n' "$result" > "$task_root/base-right-launch-r2/exit-code"
exit "$result"
