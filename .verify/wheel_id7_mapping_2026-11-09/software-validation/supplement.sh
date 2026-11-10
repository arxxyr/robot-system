#!/usr/bin/env bash
set -euo pipefail
mapping_root=/home/loosqk/robot-checks/wheel-id7-mapping-20261109
cd "$mapping_root/software-source"
export PATH=/home/loosqk/.cargo/bin:/home/loosqk/.local/bin:$PATH
export CARGO_TARGET_DIR=/home/loosqk/robot-checks/xlerobot-velocity-foundation-20261008-r7/build
export CARGO_INCREMENTAL=0
export CARGO_BUILD_JOBS=4
trap 'result=$?; printf "%s\n" "$result" > "$mapping_root/software-validation/supplement-exit-code"' EXIT
cargo test --locked --offline -p robot-s0 --features xlerobot-real --lib serial_tick::feetech_tests > "$mapping_root/software-validation/s0-wire-corrected.log" 2>&1
cargo build --locked --offline -p robot-adapter-feetech --features real-hw --example feetech_poke > "$mapping_root/software-validation/build-poke.log" 2>&1
