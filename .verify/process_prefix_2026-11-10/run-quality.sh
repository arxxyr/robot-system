#!/usr/bin/env bash
set -euo pipefail
check_root=/home/loosqk/robot-checks/process-prefix-20261110
cd "$check_root/source"
export PATH=/home/loosqk/.cargo/bin:/home/loosqk/.local/bin:$PATH
export CARGO_TARGET_DIR=/home/loosqk/robot-checks/xlerobot-velocity-foundation-20261008-r7/build
export CARGO_INCREMENTAL=0
export CARGO_BUILD_JOBS=4
mkdir -p "$check_root/quality"
trap 'result=$?; printf "%s\n" "$result" > "$check_root/quality/exit-code"' EXIT
rustc --version > "$check_root/quality/rustc.log"
cargo fmt --all > "$check_root/quality/fmt.log" 2>&1
cargo clippy --locked --offline --all --all-targets -- -D warnings > "$check_root/quality/clippy.log" 2>&1
cargo clippy --locked --offline -p robot-s0 --features xlerobot-real --all-targets -- -D warnings > "$check_root/quality/real-hw-clippy.log" 2>&1
cargo clippy --locked --offline -p robot-adapter-feetech --features real-hw --example feetech_poke -- -D warnings > "$check_root/quality/feetech-clippy.log" 2>&1
cargo test --locked --offline -p robot-logging > "$check_root/quality/logging-test.log" 2>&1
cargo build --locked --offline -p robot-cli --bin robotctl > "$check_root/quality/build-cli.log" 2>&1
strace -o "$check_root/quality/cli-startup.strace" -e trace=openat,write "$CARGO_TARGET_DIR/debug/robotctl" --help > "$check_root/quality/cli-help.log" 2>&1
