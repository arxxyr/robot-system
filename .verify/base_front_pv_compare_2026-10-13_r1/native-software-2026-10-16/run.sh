#!/usr/bin/env bash
set -euo pipefail
cd /home/loosqk/robot-checks/base-front-pv-compare-software-20261016
finish() {
  status=$?
  trap - EXIT
  printf '%s\n' "$status" > shell.exit
  date -u +%FT%TZ > finished.utc
  exit "$status"
}
trap finish EXIT
printf '%s\n' "$$" > runner.pid
date -u +%FT%TZ > started.utc
sha256sum --check input-sha256.txt > input-check-before.stdout 2> input-check-before.stderr
uv_bin=/home/loosqk/.local/bin/uv
py_bin=/home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3
"$uv_bin" --version > runtime.txt
"$py_bin" --version >> runtime.txt
candidate=.verify/base_front_pv_compare_2026-10-13_r1/candidate
run_case() {
  case_name=$1
  expected=$2
  shift 2
  set +e
  timeout --kill-after=2s 30s "$uv_bin" run --no-project --offline --python "$py_bin" "$@" > "$case_name.stdout" 2> "$case_name.stderr"
  actual=$?
  set -e
  printf '%s\n' "$actual" > "$case_name.exit"
  test "$actual" -eq "$expected"
}
run_case unittest 0 python "$candidate/test_base_front_pv_compare_r1.py"
run_case default-cli 0 python "$candidate/base_front_pv_compare_r1.py"
run_case unbound-cli 2 python "$candidate/base_front_pv_compare_r1.py" --execute --config /missing
sha256sum --check input-sha256.txt > input-check-after.stdout 2> input-check-after.stderr
