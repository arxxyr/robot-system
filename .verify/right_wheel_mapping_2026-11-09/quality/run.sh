#!/usr/bin/env bash
set -euo pipefail
mapping_root=/home/loosqk/robot-checks/right-wheel-mapping-20261109
cd "$mapping_root/source-quality"
export PATH=/home/loosqk/.cargo/bin:/home/loosqk/.local/bin:$PATH
export CARGO_TARGET_DIR=/home/loosqk/robot-checks/xlerobot-velocity-foundation-20261008-r7/build
export CARGO_INCREMENTAL=0
export CARGO_BUILD_JOBS=4
trap 'result=$?; printf "%s\n" "$result" > "$mapping_root/quality/exit-code"' EXIT
git rev-parse HEAD > "$mapping_root/quality/source-commit"
rustc --version > "$mapping_root/quality/rustc.log"
cargo fmt --all > "$mapping_root/quality/fmt.log" 2>&1
cargo clippy --locked --offline --all --all-targets -- -D warnings > "$mapping_root/quality/clippy.log" 2>&1
git diff --exit-code > "$mapping_root/quality/source-unchanged.log"
