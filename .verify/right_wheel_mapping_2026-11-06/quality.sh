#!/usr/bin/env bash
set -euo pipefail
task_root=/home/loosqk/robot-checks/right-wheel-mapping-20261106
cd "$task_root/source"
export PATH=/home/loosqk/.cargo/bin:/home/loosqk/.local/bin:$PATH
export CARGO_TARGET_DIR=/home/loosqk/robot-checks/xlerobot-velocity-foundation-20261008-r7/build
export CARGO_INCREMENTAL=0
export CARGO_BUILD_JOBS=4
mkdir "$task_root/quality"
trap 'result=$?; printf "%s\n" "$result" > "$task_root/quality/exit-code"' EXIT
git rev-parse HEAD > "$task_root/quality/source-commit"
rustc --version > "$task_root/quality/rustc.log"
cargo fmt --all > "$task_root/quality/fmt.log" 2>&1
cargo clippy --locked --offline --all --all-targets -- -D warnings > "$task_root/quality/clippy.log" 2>&1
git diff --exit-code > "$task_root/quality/source-unchanged.log"
