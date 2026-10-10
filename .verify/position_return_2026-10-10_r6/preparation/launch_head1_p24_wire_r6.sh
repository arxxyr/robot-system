#!/bin/bash
set -eu
export PYTHONDONTWRITEBYTECODE=1
tmux set-option -w -t "$TMUX_PANE" remain-on-exit on
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 /home/loosqk/robot-checks/position-return-20261010-r1/position-return-r6/p24_trial_head1_wire_r6.py --maintenance /home/loosqk/robot-checks/position-return-20261010-r1/software-facts-r1/scripts/run_hardware_maintenance.py --bindings /home/loosqk/robot-checks/position-return-20261010-r1/position-return-r6/maintenance_bindings_p24_wire_r6.json --output /home/loosqk/robot-checks/position-return-20261010-r1/position-return-r6/head1-p24-wire-r6 --run-id head1-p24-wire-r6 --artifact-root /home/loosqk/robot-checks/position-return-20261010-r1 --execute --approval /home/loosqk/robot-checks/position-return-20261010-r1/position-return-r6/head1-p24-wire-r6.go.json > /home/loosqk/robot-checks/position-return-20261010-r1/position-return-r6/head1-p24-wire-launch-r6/driver.log 2>&1
