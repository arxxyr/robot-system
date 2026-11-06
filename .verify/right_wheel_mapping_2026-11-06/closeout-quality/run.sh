#!/usr/bin/env bash
set -euo pipefail
task_root=/home/loosqk/robot-checks/right-wheel-mapping-20261106
cd "$task_root/source-closeout"
export PATH=/home/loosqk/.cargo/bin:/home/loosqk/.local/bin:$PATH
export CARGO_TARGET_DIR=/home/loosqk/robot-checks/xlerobot-velocity-foundation-20261008-r7/build
export CARGO_INCREMENTAL=0
export CARGO_BUILD_JOBS=4
trap 'result=$?; printf "%s\n" "$result" > "$task_root/closeout-quality/exit-code"' EXIT
git rev-parse HEAD > "$task_root/closeout-quality/source-commit"
rustc --version > "$task_root/closeout-quality/rustc.log"
cargo fmt --all > "$task_root/closeout-quality/fmt.log" 2>&1
cargo clippy --locked --offline --all --all-targets -- -D warnings > "$task_root/closeout-quality/clippy.log" 2>&1
git diff --exit-code > "$task_root/closeout-quality/source-unchanged.log"
