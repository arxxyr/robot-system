#!/usr/bin/env bash
set -u
task_root=/home/loosqk/robot-checks/front-wheel-name-20261105
printf '%s\n' "$$" > "$task_root/launch-hardware-r1/pid"
date -u +%Y-%m-%dT%H:%M:%S.%NZ > "$task_root/launch-hardware-r1/started-at"
export PATH=/home/loosqk/.local/bin:/home/loosqk/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
export PYTHONDONTWRITEBYTECODE=1
/home/loosqk/.local/bin/uv run --no-project --python /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 /home/loosqk/robot-checks/s0-bus2-raw-maintenance-20261023-r1/software/scripts/run_hardware_maintenance.py \
  --bindings "$task_root/mixed-bindings.json" \
  --output "$task_root/mixed-hardware-r1" \
  --run-id bus2-mixed-front-name-r1 \
  --artifact-root "$task_root" \
  --bus2-mixed-raw-hold --execute --approval "$task_root/mixed-go-r1.json" \
  > "$task_root/launch-hardware-r1/stdout.log" 2> "$task_root/launch-hardware-r1/stderr.log"
result=$?
printf '%s\n' "$result" > "$task_root/launch-hardware-r1/exit-code"
exit "$result"
