#!/usr/bin/env bash
set -euo pipefail
report_root=/home/loosqk/robot-checks/arm-range-readonly-20261110
cd "$report_root/report-source"
export PATH=/home/loosqk/.cargo/bin:/home/loosqk/.local/bin:$PATH
export CARGO_TARGET_DIR=/home/loosqk/robot-checks/xlerobot-velocity-foundation-20261008-r7/build
export CARGO_INCREMENTAL=0
export CARGO_BUILD_JOBS=4
trap 'result=$?; printf "%s\n" "$result" > "$report_root/report-quality/exit-code"' EXIT
cargo fmt --all > "$report_root/report-quality/fmt.log" 2>&1
cargo clippy --locked --offline --all --all-targets -- -D warnings > "$report_root/report-quality/clippy.log" 2>&1
