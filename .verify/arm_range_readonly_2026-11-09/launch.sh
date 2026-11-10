#!/usr/bin/env bash
set -u
printf "%s\n" "$$" > /home/loosqk/robot-checks/arm-range-readonly-20261110/launch/pid
date -u +%Y-%m-%dT%H:%M:%S.%NZ > /home/loosqk/robot-checks/arm-range-readonly-20261110/launch/started-at
export PYTHONDONTWRITEBYTECODE=1
/home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 -I -S -B /home/loosqk/robot-checks/arm-range-readonly-20261110/run_hardware_maintenance.py --bus-config /home/loosqk/robot-checks/arm-range-readonly-20261110/xlerobot_bus2_full.toml --bindings /home/loosqk/robot-checks/front-wheel-name-20261105/mixed-bindings.json --output /home/loosqk/robot-checks/arm-range-readonly-20261110/capture --run-id bus2-arm-range-readonly --artifact-root /home/loosqk/robot-checks/arm-range-readonly-20261110 --bus2-arm-range-only --execute --approval /home/loosqk/robot-checks/arm-range-readonly-20261110/go.json > /home/loosqk/robot-checks/arm-range-readonly-20261110/launch/stdout.log 2> /home/loosqk/robot-checks/arm-range-readonly-20261110/launch/stderr.log
result=$?
printf "%s\n" "$result" > /home/loosqk/robot-checks/arm-range-readonly-20261110/launch/exit-code
exit "$result"
