#!/usr/bin/env bash
set -euo pipefail
check_root=/home/loosqk/robot-checks/arm-range-readonly-20261110
cd "$check_root/source"
export PATH=/home/loosqk/.cargo/bin:/home/loosqk/.local/bin:$PATH
export CARGO_TARGET_DIR=/home/loosqk/robot-checks/xlerobot-velocity-foundation-20261008-r7/build
export CARGO_INCREMENTAL=0
export CARGO_BUILD_JOBS=4
trap 'result=$?; printf "%s\n" "$result" > "$check_root/software-validation/exit-code"' EXIT
rustc --version > "$check_root/software-validation/rustc.log"
cargo fmt --all > "$check_root/software-validation/fmt.log" 2>&1
cargo clippy --locked --offline --all --all-targets -- -D warnings > "$check_root/software-validation/clippy.log" 2>&1
SDK_GOLDEN_SITE=/home/loosqk/repo/dimensional-applications/.venv/lib/python3.12/site-packages \
ROBOT_MIXED_SOFTWARE_FIXTURE=/home/loosqk/robot-checks/wheel-config-mapping-20261110/software-validation/mixed-fixture.json \
/home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 -I -S -B \
  scripts/tests/test_hardware_maintenance.py > "$check_root/software-validation/python.log" 2>&1
