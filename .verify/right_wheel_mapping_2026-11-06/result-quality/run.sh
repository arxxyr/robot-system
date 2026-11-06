#!/usr/bin/env bash
set -euo pipefail
task_root=/home/loosqk/robot-checks/right-wheel-mapping-20261106
cd "$task_root/source-result"
export PATH=/home/loosqk/.cargo/bin:/home/loosqk/.local/bin:$PATH
export CARGO_TARGET_DIR=/home/loosqk/robot-checks/xlerobot-velocity-foundation-20261008-r7/build
export CARGO_INCREMENTAL=0
export CARGO_BUILD_JOBS=4
trap 'result=$?; printf "%s\n" "$result" > "$task_root/result-quality/exit-code"' EXIT
git rev-parse HEAD > "$task_root/result-quality/source-commit"
rustc --version > "$task_root/result-quality/rustc.log"
cargo fmt --all > "$task_root/result-quality/fmt.log" 2>&1
cargo clippy --locked --offline --all --all-targets -- -D warnings > "$task_root/result-quality/clippy.log" 2>&1
git diff --exit-code > "$task_root/result-quality/source-unchanged.log"
