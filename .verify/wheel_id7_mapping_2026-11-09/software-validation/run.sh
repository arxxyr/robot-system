#!/usr/bin/env bash
set -euo pipefail
mapping_root=/home/loosqk/robot-checks/wheel-id7-mapping-20261109
cd "$mapping_root/software-source"
export PATH=/home/loosqk/.cargo/bin:/home/loosqk/.local/bin:$PATH
export CARGO_TARGET_DIR=/home/loosqk/robot-checks/xlerobot-velocity-foundation-20261008-r7/build
export CARGO_INCREMENTAL=0
export CARGO_BUILD_JOBS=4
trap 'result=$?; printf "%s\n" "$result" > "$mapping_root/software-validation/exit-code"' EXIT
rustc --version > "$mapping_root/software-validation/rustc.log"
cargo fmt --all > "$mapping_root/software-validation/fmt.log" 2>&1
cargo clippy --locked --offline --all --all-targets -- -D warnings > "$mapping_root/software-validation/clippy.log" 2>&1
cargo clippy --locked --offline -p robot-adapter-feetech --features real-hw --all-targets -- -D warnings > "$mapping_root/software-validation/clippy-real-hw.log" 2>&1
cargo test --locked --offline -p robot-kinematics-xlerobot --lib > "$mapping_root/software-validation/kinematics.log" 2>&1
cargo test --locked --offline -p robot-adapter-feetech --features real-hw --lib > "$mapping_root/software-validation/feetech.log" 2>&1
cargo test --locked --offline -p robot-adapter-feetech --features real-hw --example feetech_poke > "$mapping_root/software-validation/poke.log" 2>&1
cargo test --locked --offline -p robot-s0 --features xlerobot-real --lib grouped_position > "$mapping_root/software-validation/s0-groups.log" 2>&1
cargo test --locked --offline -p robot-s0 --features xlerobot-real --lib serial_tick_feetech_tests > "$mapping_root/software-validation/s0-wire.log" 2>&1
cargo test --locked --offline -p robot-s0 --features xlerobot-real --lib serial_io_thread > "$mapping_root/software-validation/s0-worker.log" 2>&1
SDK_GOLDEN_SITE=/home/loosqk/repo/dimensional-applications/.venv/lib/python3.12/site-packages /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 -I -S scripts/tests/test_hardware_maintenance.py > "$mapping_root/software-validation/python.log" 2>&1
