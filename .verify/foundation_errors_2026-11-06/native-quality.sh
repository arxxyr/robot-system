#!/usr/bin/env bash
set -euo pipefail
task_root=/home/loosqk/robot-checks/foundation-errors-20261106
cd "$task_root/source"
export PATH=/home/loosqk/.cargo/bin:/home/loosqk/.local/bin:$PATH
export CARGO_TARGET_DIR=/home/loosqk/robot-checks/xlerobot-velocity-foundation-20261008-r7/build
export CARGO_INCREMENTAL=0
export CARGO_BUILD_JOBS=4
task_logs="$task_root/run2"
mkdir -p "$task_logs"
trap 'result=$?; printf "%s\n" "$result" > "$task_logs/exit-code"' EXIT
rustc --version > "$task_logs/rustc.log"
sha256sum --check "$task_root/candidate.sha256" > "$task_logs/source-before.log"
cargo fmt --all > "$task_logs/fmt.log" 2>&1
cargo clippy --locked --offline --all --all-targets -- -D warnings > "$task_logs/clippy-root.log" 2>&1
cargo clippy --locked --offline -p robot-adapter-feetech --features real-hw --all-targets -- -D warnings > "$task_logs/clippy-feetech.log" 2>&1
cargo test --locked --offline -p robot-core -p robot-s1 -p robot-adapter-feetech -p robot-health -p robot-rpc -p robot-viz --lib -- --test-threads=1 > "$task_logs/unit-tests.log" 2>&1
cargo test --locked --offline -p robot-s0 --lib serial_tick:: -- --test-threads=1 > "$task_logs/serial-tick-tests.log" 2>&1
cargo test --locked --offline -p robot-adapter-feetech --features real-hw --example feetech_poke -- --test-threads=1 > "$task_logs/maintenance-tests.log" 2>&1
uv run --no-project python scripts/tests/test_hardware_maintenance.py MixedRawTests ClassificationTests > "$task_logs/python-tests.log" 2>&1
cargo build --locked --offline -p robot-adapter-feetech --features real-hw --example feetech_poke --message-format=json > "$task_logs/example-build.jsonl" 2> "$task_logs/example-build.log"
uv run --no-project python .verify/foundation_errors_2026-11-06/check_early_receipt.py "$task_logs" > "$task_logs/early-parser.log" 2>&1
rm "$task_logs/example-build.jsonl"
sha256sum --check "$task_root/candidate.sha256" > "$task_logs/source-after.log"
