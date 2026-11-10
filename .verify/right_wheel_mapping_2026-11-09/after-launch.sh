#!/usr/bin/env bash
set -u
printf "%s\n" "$$" > /home/loosqk/robot-checks/right-wheel-mapping-20261109/after-launch/pid
date -u +%Y-%m-%dT%H:%M:%S.%NZ > /home/loosqk/robot-checks/right-wheel-mapping-20261109/after-launch/started-at
export PYTHONDONTWRITEBYTECODE=1
/home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 -I -S /home/loosqk/robot-checks/s0-bus2-raw-maintenance-20261023-r1/software/scripts/run_hardware_maintenance.py --bindings /home/loosqk/robot-checks/front-wheel-name-20261105/mixed-bindings.json --output /home/loosqk/robot-checks/right-wheel-mapping-20261109/after-capture --run-id right9-mapping-after --artifact-root /home/loosqk/robot-checks/right-wheel-mapping-20261109 --right-wheel-feedback-only --execute --approval /home/loosqk/robot-checks/right-wheel-mapping-20261109/after-go.json > /home/loosqk/robot-checks/right-wheel-mapping-20261109/after-launch/stdout.log 2> /home/loosqk/robot-checks/right-wheel-mapping-20261109/after-launch/stderr.log
result=$?
printf "%s\n" "$result" > /home/loosqk/robot-checks/right-wheel-mapping-20261109/after-launch/exit-code
exit "$result"
